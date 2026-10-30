//! The workspace: tiles, each a framed card. Up to four sit side by side;
//! more stack in those four columns (the fifth under the first, the sixth
//! under the second, …), and in a column of three or more the tile last
//! used there takes half its height (see [`arrange`]).
//!
//! A tile has at most two panes: a list (or whatever it was opened on) and
//! the item opened from it. One rule: a click inside a tile opens in that
//! tile's right pane; the header's navigation opens a tile of its own (the
//! one already showing it, else a new one); ⌘/Ctrl-click (or a tile's ⧉)
//! makes a new tile. Each tile keeps its own
//! back and forward history (⌘[ and ⌘]).
//!
//! On a phone one tile shows at a time (a switcher at the bottom) and one
//! pane of it, with back in its header.
//!
//! The layout is saved in the browser; the URL shows the focused column.
//! It also tracks attention: sessions that finished or failed while not
//! shown, and pinned sessions, for the dock, the inbox and mission control.

use std::collections::{HashMap, HashSet};

use c6_api::{Session, SessionStatus};
use leptos::html::Div;
use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsCast;

use crate::api::{self, LiveFeed};
use crate::util::{storage_get, storage_set};

/// Tiles side by side before they stack.
pub const COLUMNS: usize = 4;
/// Finished sessions kept for you to look at.
const MAX_ATTENTION: usize = 20;

/// What a column shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "view", rename_all = "snake_case")]
pub enum View {
    Sessions,
    Session {
        id: String,
        /// Where to go once it loads (from a search hit).
        #[serde(default)]
        target: Option<Target>,
    },
    Search {
        q: String,
        #[serde(default)]
        kind: String,
        #[serde(default)]
        worker: String,
    },
    Workers,
    Worker {
        id: String,
    },
    Settings {
        /// A notice after a redirect: "chatgpt" or "link".
        #[serde(default)]
        note: Option<String>,
        /// Which section ("" for the first).
        #[serde(default)]
        section: String,
    },
    Inbox,
    Mission,
    Usage,
    Analytics,
    Activity,
    Memex,
    Mail,
    Chat,
    /// Mail & chat's home: what needs you (docs/brief.md).
    Brief,
    Calendar,
    Contacts,
    Photos,
    Decisions,
    /// Work you mean to finish (docs/work.md); with an id, that piece.
    Work {
        #[serde(default)]
        id: String,
    },
    /// Collections; one by name, or (empty) the list.
    Collections {
        #[serde(default)]
        name: String,
    },
    /// Every realm, and the grants between them (docs/explorer.md).
    Realms,
    /// One realm in the explorer: a section of its inventory, an object.
    Explore {
        realm: String,
        #[serde(default)]
        section: String,
        #[serde(default)]
        kind: String,
        #[serde(default)]
        id: String,
    },
    /// A realm's view (docs/os.md §4.5); with a record, that record.
    App {
        name: String,
        #[serde(default)]
        record: String,
    },
}

/// A place in a session: the transcript element to reveal, the side chats
/// to expand on the way, and the query to highlight.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub q: String,
    #[serde(default)]
    pub anchor: Option<String>,
    #[serde(default)]
    pub open: Vec<String>,
    /// The session's event that shows it, when known.
    #[serde(default)]
    pub at: Option<u64>,
}

fn enc(s: &str) -> String {
    js_sys::encode_uri_component(s).as_string().unwrap_or_default()
}

impl View {
    pub fn session(id: impl Into<String>) -> Self {
        View::Session { id: id.into(), target: None }
    }

    /// The URL for this view (shown in the address bar, and for copying).
    /// Where a page starts: a realm's sessions, or the console's realms.
    pub fn home() -> Self {
        if crate::place::is_console() { View::Realms } else { View::Sessions }
    }

    /// The view's address on this page (within its realm or the console).
    pub fn href(&self) -> String {
        crate::place::href(&self.path())
    }

    pub fn path(&self) -> String {
        match self {
            View::Sessions => "/".into(),
            View::Session { id, target: None } => format!("/sessions/{id}"),
            View::Session { id, target: Some(t) } => {
                let mut p = format!("/sessions/{id}?q={}", enc(&t.q));
                if !t.open.is_empty() {
                    p.push_str(&format!("&open={}", enc(&t.open.join(","))));
                }
                if let Some(a) = &t.anchor {
                    p.push_str(&format!("#{a}"));
                }
                p
            }
            View::Search { q, kind, worker } => {
                let mut p = format!("/search?q={}", enc(q));
                if !kind.is_empty() {
                    p.push_str(&format!("&kind={}", enc(kind)));
                }
                if !worker.is_empty() {
                    p.push_str(&format!("&worker={}", enc(worker)));
                }
                p
            }
            View::Workers => "/workers".into(),
            View::Worker { id } => format!("/workers/{id}"),
            View::Settings { section, .. } if section.is_empty() => "/settings".into(),
            View::Settings { section, .. } => format!("/settings/{section}"),
            View::Inbox => "/inbox".into(),
            View::Mission => "/mission".into(),
            View::Usage => "/usage".into(),
            View::Analytics => "/analytics".into(),
            View::Activity => "/activity".into(),
            View::Memex => "/memex".into(),
            View::Mail => "/mail".into(),
            View::Chat => "/chat".into(),
            View::Brief => "/brief".into(),
            View::Calendar => "/calendar".into(),
            View::Contacts => "/contacts".into(),
            View::Photos => "/photos".into(),
            View::Decisions => "/decisions".into(),
            View::Work { id } if id.is_empty() => "/work".into(),
            View::Work { id } => format!("/work/{id}"),
            View::Collections { name } if name.is_empty() => "/collections".into(),
            View::Collections { name } => format!("/collections/{name}"),
            View::Realms => "/realms".into(),
            View::Explore { realm, section, kind, id } => match (kind.is_empty(), section.is_empty()) {
                (true, true) => format!("/realms/{}", encode(realm)),
                (true, false) => format!("/realms/{}/in/{}", encode(realm), encode(section)),
                (false, true) => format!("/realms/{}/{}/{}", encode(realm), encode(kind), encode(id)),
                (false, false) => format!("/realms/{}/{}/{}?in={}", encode(realm), encode(kind), encode(id), encode(section)),
            },
            View::App { name, record } if record.is_empty() => format!("/views/{name}"),
            View::App { name, record } => format!("/views/{name}/{record}"),
        }
    }

    /// The view at the page's URL (on load, from links and redirects).
    pub fn from_location() -> Self {
        let loc = window().location();
        let path = loc.pathname().unwrap_or_default();
        let query: HashMap<String, String> = web_sys::UrlSearchParams::new_with_str(&loc.search().unwrap_or_default())
            .ok()
            .map(|p| {
                let mut m = HashMap::new();
                for key in ["q", "kind", "worker", "open", "signed_in", "signed_in_with_link"] {
                    if let Some(v) = p.get(key) {
                        m.insert(key.to_owned(), v);
                    }
                }
                m
            })
            .unwrap_or_default();
        let hash = loc.hash().unwrap_or_default().trim_start_matches('#').to_owned();
        Self::parse(&crate::place::strip(&path), &query, &hash)
    }

    /// The view at a path of carbon6 (e.g. a link the head gives).
    pub fn from_path(href: &str) -> Self {
        let (path, q) = href.split_once('?').unwrap_or((href, ""));
        let query: HashMap<String, String> =
            q.split('&').filter_map(|p| p.split_once('=')).map(|(k, v)| (k.to_owned(), decode(v))).collect();
        Self::parse(path, &query, "")
    }

    fn parse(path: &str, query: &HashMap<String, String>, hash: &str) -> Self {
        let hash = hash.to_owned();
        let get = |k: &str| query.get(k).cloned().unwrap_or_default();
        let parts: Vec<String> = path.trim_matches('/').split('/').map(decode).collect();
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        match parts.as_slice() {
            ["sessions", id] => {
                let target = (query.contains_key("q") || !hash.is_empty()).then(|| Target {
                    q: get("q"),
                    anchor: Some(hash).filter(|h| !h.is_empty()),
                    open: query
                        .get("open")
                        .map(|o| o.split(',').filter(|s| !s.is_empty()).map(str::to_owned).collect())
                        .unwrap_or_default(),
                    at: None,
                });
                View::Session { id: (*id).to_owned(), target }
            }
            ["search"] => View::Search { q: get("q"), kind: get("kind"), worker: get("worker") },
            ["workers"] => View::Workers,
            ["workers", id] => View::Worker { id: (*id).to_owned() },
            ["settings", rest @ ..] => {
                let note = if query.contains_key("signed_in") {
                    Some("chatgpt".into())
                } else if query.contains_key("signed_in_with_link") {
                    Some("link".into())
                } else {
                    None
                };
                // Where a redirect back lands.
                let section = match (rest.first(), note.as_deref()) {
                    (Some(s), _) => (*s).to_owned(),
                    (None, Some("chatgpt")) => "models".into(),
                    (None, Some("link")) => "account".into(),
                    (None, None) if query.contains_key("google") => "mail".into(),
                    _ => String::new(),
                };
                View::Settings { note, section }
            }
            ["inbox"] => View::Inbox,
            ["mission"] => View::Mission,
            ["usage"] => View::Usage,
            ["analytics"] => View::Analytics,
            ["activity"] => View::Activity,
            ["memex"] => View::Memex,
            ["mail"] => View::Mail,
            ["chat"] => View::Chat,
            ["brief"] => View::Brief,
            ["calendar"] => View::Calendar,
            ["contacts"] => View::Contacts,
            ["photos"] => View::Photos,
            ["decisions"] | ["todos"] => View::Decisions,
            ["work"] => View::Work { id: String::new() },
            ["work", id] => View::Work { id: (*id).to_owned() },
            ["collections"] => View::Collections { name: String::new() },
            ["collections", name] => View::Collections { name: (*name).to_owned() },
            ["views", name] => View::App { name: (*name).to_owned(), record: String::new() },
            ["views", name, record] => View::App { name: (*name).to_owned(), record: (*record).to_owned() },
            ["realms"] => View::Realms,
            ["realms", realm] => {
                View::Explore { realm: (*realm).to_owned(), section: String::new(), kind: String::new(), id: String::new() }
            }
            ["realms", realm, "in", section] => {
                View::Explore { realm: (*realm).to_owned(), section: (*section).to_owned(), kind: String::new(), id: String::new() }
            }
            ["realms", realm, kind, id] => {
                View::Explore { realm: (*realm).to_owned(), section: get("in"), kind: (*kind).to_owned(), id: (*id).to_owned() }
            }
            _ => View::home(),
        }
    }

    /// A short name, for column headers, spines and the tile switcher.
    pub fn label(&self, live: &LiveFeed) -> String {
        match self {
            View::Sessions => "Sessions".into(),
            View::Session { id, .. } => {
                live.sessions.with(|ss| ss.iter().find(|s| &s.id == id).map(|s| s.title.clone())).unwrap_or_else(|| "Session".into())
            }
            View::Search { q, .. } => format!("Search: {q}"),
            View::Workers => "Workers".into(),
            View::Worker { id } => live
                .workers
                .with(|ws| ws.iter().find(|w| &w.id.to_string() == id).map(|w| w.name.clone()))
                .unwrap_or_else(|| "Worker".into()),
            View::Settings { section, .. } => match crate::settings::section_title(section) {
                Some(t) => format!("Settings · {t}"),
                None => "Settings".into(),
            },
            View::Inbox => "Inbox".into(),
            View::Mission => "Mission control".into(),
            View::Usage => "Usage".into(),
            View::Analytics => "Analytics".into(),
            View::Activity => "Activity".into(),
            View::Memex => "Memex".into(),
            View::Mail => "Mail".into(),
            View::Chat => "Chat".into(),
            View::Brief => "Brief".into(),
            View::Calendar => "Calendar".into(),
            View::Contacts => "Contacts".into(),
            View::Photos => "Photos".into(),
            View::Decisions => "Decisions".into(),
            View::Work { id } if id.is_empty() => "Work".into(),
            View::Work { .. } => "Work item".into(),
            View::Collections { name } if name.is_empty() => "Collections".into(),
            View::Collections { name } => format!("Collection · {name}"),
            View::Realms => "Realms".into(),
            View::Explore { realm, .. } => format!("Explore · {realm}"),
            View::App { name, record } if record.is_empty() => name.clone(),
            View::App { name, .. } => format!("{name} · record"),
        }
    }

    /// A top-level section (not one session or worker): the header's nav
    /// and search swap one section for another in place instead of
    /// stacking columns.
    fn is_section(&self) -> bool {
        !matches!(self, View::Session { .. } | View::Worker { .. })
            && !matches!(self, View::Collections { name } if !name.is_empty())
            && !matches!(self, View::Work { id } if !id.is_empty())
            && !matches!(self, View::App { record, .. } if !record.is_empty())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub key: u64,
    pub view: View,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Tile {
    pub key: u64,
    /// Its panes: what it was opened on, and the item opened from it (at
    /// most two).
    pub columns: Vec<Column>,
    /// The pane a phone shows (a wide screen shows both).
    pub cursor: usize,
    /// The pane last interacted with: keyboard shortcuts act on it.
    pub active: u64,
    /// When the tile was last focused (a counter), to pick the least
    /// recently used one to replace when all tiles are in use.
    #[serde(default)]
    pub used: u64,
    /// Its history: what it showed before (back) and after (forward).
    #[serde(default)]
    pub back: Vec<Vec<Column>>,
    #[serde(default)]
    pub forward: Vec<Vec<Column>>,
    /// The column of the workspace it sits in (0 the leftmost), placed
    /// when it opens and moved by dragging its bar.
    #[serde(default)]
    pub stack: Option<usize>,
}

/// How far back a tile remembers.
const HISTORY: usize = 30;

impl Tile {
    /// Shows `columns` instead, remembering what it showed.
    fn go(&mut self, columns: Vec<Column>) {
        let before = std::mem::replace(&mut self.columns, columns);
        self.back.push(before);
        if self.back.len() > HISTORY {
            self.back.remove(0);
        }
        self.forward.clear();
        self.settle();
    }

    /// Shows its last pane, and makes it the active one.
    fn settle(&mut self) {
        self.cursor = self.columns.len() - 1;
        self.active = self.columns[self.cursor].key;
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub tiles: Vec<Tile>,
    /// Index of the focused tile.
    pub focus: usize,
    pub next: u64,
}

impl Layout {
    fn new(view: View) -> Self {
        let mut l = Layout { tiles: Vec::new(), focus: 0, next: 1 };
        let t = l.new_tile(view);
        l.tiles.push(t);
        l
    }

    fn key(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    fn new_tile(&mut self, view: View) -> Tile {
        let (tk, ck) = (self.key(), self.key());
        Tile {
            key: tk,
            columns: vec![Column { key: ck, view }],
            cursor: 0,
            active: ck,
            used: self.next,
            back: Vec::new(),
            forward: Vec::new(),
            stack: None,
        }
    }

    /// Puts tiles without a column in one: a new column while there are
    /// fewer than [`COLUMNS`], then the column with the fewest (the
    /// leftmost of those), so they fill left to right and then stack in
    /// turn. Columns are numbered from 0 without gaps.
    fn place(&mut self) {
        for i in 0..self.tiles.len() {
            if self.tiles[i].stack.is_some() {
                continue;
            }
            let mut count = [0usize; COLUMNS];
            for t in &self.tiles {
                if let Some(c) = t.stack.filter(|&c| c < COLUMNS) {
                    count[c] += 1;
                }
            }
            let used = count.iter().filter(|&&n| n > 0).count();
            let stack = if used < COLUMNS {
                count.iter().position(|&n| n == 0).unwrap_or(0)
            } else {
                (0..COLUMNS).min_by_key(|&c| (count[c], c)).unwrap_or(0)
            };
            self.tiles[i].stack = Some(stack);
        }
        let mut stacks: Vec<usize> = self.tiles.iter().filter_map(|t| t.stack).collect();
        stacks.sort_unstable();
        stacks.dedup();
        for t in &mut self.tiles {
            t.stack = t.stack.and_then(|c| stacks.iter().position(|&x| x == c)).map(|c| c.min(COLUMNS - 1));
        }
    }

    /// Moves tile `key` to where it was dropped, keeping focus where it was.
    fn move_tile(&mut self, key: u64, to: Drop) {
        let focused = self.tiles.get(self.focus).map(|t| t.key);
        let Some(from) = self.tiles.iter().position(|t| t.key == key) else { return };
        let target = match to {
            Drop::Above(k) | Drop::Below(k) | Drop::LeftOf(k) | Drop::RightOf(k) => k,
        };
        if target == key {
            return;
        }
        let Some(target_stack) = self.tile(target).and_then(|t| t.stack) else { return };
        let columns = self.tiles.iter().filter_map(|t| t.stack).collect::<HashSet<_>>().len();
        let alone = self.tiles.iter().filter(|t| t.stack == self.tiles[from].stack).count() == 1;
        let mut tile = self.tiles.remove(from);
        match to {
            Drop::Above(_) | Drop::Below(_) => {
                tile.stack = Some(target_stack);
                let at = self.tiles.iter().position(|t| t.key == target).unwrap_or(0);
                let at = if matches!(to, Drop::Below(_)) { at + 1 } else { at };
                self.tiles.insert(at, tile);
            }
            Drop::LeftOf(_) | Drop::RightOf(_) => {
                // A new column, while there's room for one (a tile alone
                // in its column just moves its column).
                if columns >= COLUMNS && !alone {
                    self.tiles.insert(from, tile);
                    return;
                }
                // Spread the columns out (c to 2c + 1), to make room on
                // either side of each.
                for t in &mut self.tiles {
                    t.stack = t.stack.map(|c| 2 * c + 1);
                }
                tile.stack = Some(if matches!(to, Drop::LeftOf(_)) { 2 * target_stack } else { 2 * target_stack + 2 });
                self.tiles.push(tile);
            }
        }
        self.place();
        if let Some(i) = focused.and_then(|k| self.tiles.iter().position(|t| t.key == k)) {
            self.focus = i;
        }
    }

    fn touch(&mut self, ti: usize) {
        let k = self.key();
        if let Some(t) = self.tiles.get_mut(ti) {
            t.used = k;
        }
    }

    /// Where a session is open: (tile index, column index).
    fn find_session(&self, id: &str) -> Option<(usize, usize)> {
        self.tiles
            .iter()
            .enumerate()
            .find_map(|(ti, t)| t.columns.iter().position(|c| matches!(&c.view, View::Session { id: x, .. } if x == id)).map(|ci| (ti, ci)))
    }

    /// A Brief may already be beside Mail, not just first in its tile.
    fn focus_brief(&mut self) -> bool {
        let found =
            self.tiles.iter().enumerate().find_map(|(ti, t)| t.columns.iter().position(|c| c.view == View::Brief).map(|ci| (ti, ci)));
        let Some((ti, ci)) = found else { return false };
        let t = &mut self.tiles[ti];
        t.cursor = ci;
        t.active = t.columns[ci].key;
        self.focus = ti;
        self.touch(ti);
        true
    }

    /// Opens `view` in a new tile, after the others (left to right, then
    /// stacking down the columns, in the order opened).
    fn open_tile(&mut self, view: View) {
        let tile = self.new_tile(view);
        self.tiles.push(tile);
        self.place();
        self.focus = self.tiles.len() - 1;
        self.touch(self.focus);
    }

    pub fn tile(&self, key: u64) -> Option<&Tile> {
        self.tiles.iter().find(|t| t.key == key)
    }

    fn tile_mut(&mut self, key: u64) -> Option<&mut Tile> {
        self.tiles.iter_mut().find(|t| t.key == key)
    }

    fn valid(&self) -> bool {
        !self.tiles.is_empty()
            && self.focus < self.tiles.len()
            && self.tiles.iter().all(|t| !t.columns.is_empty() && t.columns.len() <= 2 && t.cursor < t.columns.len())
    }

    /// Session ids shown now (in any tile's panes).
    pub fn visible_sessions(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        for t in &self.tiles {
            for c in &t.columns {
                if let View::Session { id, .. } = &c.view {
                    out.insert(id.clone());
                }
            }
        }
        out
    }

    pub fn focused_view(&self) -> Option<&View> {
        let t = self.tiles.get(self.focus)?;
        t.columns.iter().find(|c| c.key == t.active).or_else(|| t.columns.get(t.cursor)).map(|c| &c.view)
    }
}

/// How a link opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum How {
    /// In this tile: its right pane (from inside it), or in place of it
    /// (from the header's navigation).
    Beside,
    /// In a new tile (⌘/Ctrl-click).
    NewTile,
    /// In a tile of its own: the one already showing it, or a new one (the
    /// Apps menu).
    OwnTile,
}

/// Where a dragged tile was dropped, by the tile it was dropped on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drop {
    /// In that tile's column, above or below it.
    Above(u64),
    Below(u64),
    /// In a new column, left or right of that tile's.
    LeftOf(u64),
    RightOf(u64),
}

impl How {
    pub fn from_mouse(ev: &web_sys::MouseEvent) -> Self {
        if ev.meta_key() || ev.ctrl_key() { How::NewTile } else { How::Beside }
    }
}

// v2: tiles of two panes with history (v1 tiles of column chains are dropped).
const LAYOUT_KEY: &str = "c6.workspace.v2";
const ATTENTION_KEY: &str = "c6.attention";
const PINNED_KEY: &str = "c6.pinned";

#[derive(Clone, Copy)]
pub struct Workspace {
    pub layout: RwSignal<Layout>,
    /// A request to open, consumed by the active Brief. Never part of its view.
    pub brief_request: RwSignal<Option<i64>>,
    /// What each session pane offers its tile's bar (its details), by pane
    /// key, while it is shown.
    pub chrome: RwSignal<HashMap<u64, crate::session::SessionChrome>>,
    /// Sessions that finished or failed while not shown, newest first.
    pub attention: RwSignal<Vec<String>>,
    pub pinned: RwSignal<Vec<String>>,
    /// The tile being dragged by its bar, and where it would drop.
    pub dragging: RwSignal<Option<u64>>,
    pub drop_at: RwSignal<Option<Drop>>,
}

fn load<T: for<'de> Deserialize<'de>>(key: &str) -> Option<T> {
    serde_json::from_str(&storage_get(key)?).ok()
}

impl Workspace {
    /// The saved workspace, with the page's URL opened in it.
    pub fn restore() -> Self {
        let here = View::from_location();
        let request = (here == View::Brief)
            .then(|| {
                web_sys::UrlSearchParams::new_with_str(&window().location().search().unwrap_or_default())
                    .ok()
                    .and_then(|q| q.get("request"))
                    .and_then(|v| crate::brief::request_id(&v))
            })
            .flatten();
        let layout = match load::<Layout>(LAYOUT_KEY).filter(Layout::valid) {
            None => Layout::new(here.clone()),
            Some(mut l) => {
                l.place();
                l
            }
        };
        let ws = Workspace {
            layout: RwSignal::new(layout),
            brief_request: RwSignal::new(request),
            chrome: RwSignal::new(HashMap::new()),
            attention: RwSignal::new(load(ATTENTION_KEY).unwrap_or_default()),
            pinned: RwSignal::new(load(PINNED_KEY).unwrap_or_default()),
            dragging: RwSignal::new(None),
            drop_at: RwSignal::new(None),
        };
        // A link or redirect to a particular view opens it (the home page
        // just restores the workspace).
        if here == View::Brief {
            ws.open(None, here, How::OwnTile);
        } else if here != View::home() && ws.layout.with_untracked(|l| l.focused_view() != Some(&here)) {
            ws.open(None, here, How::Beside);
        }
        // Save, and show the focused column in the address bar.
        Effect::new(move || {
            let l = ws.layout.get();
            storage_set(LAYOUT_KEY, &serde_json::to_string(&l).unwrap_or_default());
            if let Some(v) = l.focused_view() {
                let _ = window().history().and_then(|h| h.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&v.href())));
            }
        });
        Effect::new(move || storage_set(ATTENTION_KEY, &serde_json::to_string(&ws.attention.get()).unwrap_or_default()));
        Effect::new(move || storage_set(PINNED_KEY, &serde_json::to_string(&ws.pinned.get()).unwrap_or_default()));
        ws
    }

    /// Opens `view` from pane `from` (tile key, pane key), or from outside
    /// the tiles (the header, a notification).
    ///
    /// One rule: ⌘/Ctrl-click (`NewTile`) makes a new tile; anything else
    /// stays in its tile. From a pane, it opens in the tile's right pane
    /// (beside a list, or in place of the item there). From outside, it
    /// replaces the focused tile (an item goes beside the list it shows).
    /// A session already shown somewhere is shown where it is.
    pub fn open(&self, from: Option<(u64, u64)>, view: View, how: How) {
        self.layout.update(|l| {
            if view == View::Brief && how != How::NewTile && l.focus_brief() {
                return;
            }
            let origin = from.and_then(|(t, _)| l.tiles.iter().position(|x| x.key == t)).unwrap_or(l.focus);
            if let View::Session { id, target } = &view
                && let Some((ti, ci)) = l.find_session(id)
            {
                let t = &mut l.tiles[ti];
                if target.is_some() {
                    t.columns[ci].view = view.clone(); // go to the new place in it
                }
                t.cursor = ci;
                t.active = t.columns[ci].key;
                l.focus = ti;
                l.touch(ti);
                return;
            }
            if how == How::OwnTile
                && let Some(ti) = l.tiles.iter().position(|t| t.columns[0].view == view)
            {
                l.tiles[ti].settle();
                l.focus = ti;
                l.touch(ti);
                return;
            }
            if how != How::Beside {
                l.open_tile(view);
                return;
            }
            let key = l.key();
            let t = &mut l.tiles[origin];
            let list = t.columns[0].clone();
            let next = match from {
                // Inside the tile: the item goes in the right pane.
                Some(_) => vec![list, Column { key, view }],
                // From outside: an item beside the list the tile shows, a
                // section in place of the tile.
                None if !view.is_section() && list.view.is_section() => vec![list, Column { key, view }],
                None => vec![Column { key, view }],
            };
            if t.columns.iter().map(|c| &c.view).ne(next.iter().map(|c| &c.view)) {
                t.go(next);
            }
            t.settle();
            l.focus = origin;
            l.touch(origin);
        });
    }

    /// Moves the item a tile shows (its right pane) into a tile of its own.
    pub fn detach(&self, tile: u64) {
        self.layout.update(|l| {
            let Some(ti) = l.tiles.iter().position(|t| t.key == tile) else { return };
            if l.tiles[ti].columns.len() < 2 {
                return;
            }
            let item = l.tiles[ti].columns[1].view.clone();
            let list = l.tiles[ti].columns[0].clone();
            l.tiles[ti].go(vec![list]);
            l.open_tile(item);
        });
    }

    /// Back (`delta` < 0) or forward in the focused tile's history.
    pub fn step(&self, delta: isize) {
        self.layout.update(|l| {
            let t = &mut l.tiles[l.focus];
            if delta < 0 {
                if let Some(prev) = t.back.pop() {
                    let now = std::mem::replace(&mut t.columns, prev);
                    t.forward.push(now);
                    t.settle();
                }
            } else if let Some(next) = t.forward.pop() {
                let now = std::mem::replace(&mut t.columns, next);
                t.back.push(now);
                t.settle();
            }
        });
    }

    /// On a phone, from the item back to the list beside it.
    pub fn show_list(&self, tile: u64) {
        self.layout.update(|l| {
            if let Some(t) = l.tile_mut(tile) {
                t.cursor = 0;
                t.active = t.columns[0].key;
            }
        });
    }

    /// Changes what a column shows in place (e.g. a search's query).
    pub fn replace(&self, tile: u64, col: u64, view: View) {
        self.layout.update(|l| {
            if let Some(c) = l.tile_mut(tile).and_then(|t| t.columns.iter_mut().find(|c| c.key == col)) {
                c.view = view;
            }
        });
    }

    /// Closes a pane: the item (the list stays), or the list (the item
    /// takes the tile); a tile's last pane closes the tile.
    pub fn close_column(&self, tile: u64, col: u64) {
        self.layout.update(|l| {
            let Some(ti) = l.tiles.iter().position(|t| t.key == tile) else { return };
            let t = &mut l.tiles[ti];
            let Some(i) = t.columns.iter().position(|c| c.key == col) else { return };
            if t.columns.len() > 1 {
                let mut rest = t.columns.clone();
                rest.remove(i);
                t.go(rest);
                return;
            }
            if l.tiles.len() == 1 {
                *l = Layout::new(View::home());
            } else {
                l.tiles.remove(ti);
                l.place();
                l.focus = l.focus.min(l.tiles.len() - 1);
            }
        });
    }

    pub fn close_tile(&self, tile: u64) {
        self.layout.update(|l| {
            let Some(ti) = l.tiles.iter().position(|t| t.key == tile) else { return };
            if l.tiles.len() == 1 {
                *l = Layout::new(View::home());
            } else {
                l.tiles.remove(ti);
                l.place();
                l.focus = l.focus.min(l.tiles.len() - 1);
            }
        });
    }

    /// Moves a dragged tile to where it was dropped.
    pub fn move_tile(&self, key: u64, to: Drop) {
        self.layout.update(|l| l.move_tile(key, to));
    }

    pub fn new_tile(&self) {
        self.open(None, View::home(), How::NewTile);
    }

    pub fn focus_tile(&self, index: usize) {
        self.layout.update(|l| {
            if index < l.tiles.len() && l.focus != index {
                l.focus = index;
                l.touch(index);
            }
        });
    }

    /// Focuses a tile you pressed on, without growing it yet: that waits
    /// for the click to finish ([`Workspace::grow`]), so what you clicked
    /// doesn't move away under the pointer.
    fn focus_tile_key(&self, key: u64) {
        if let Some(i) = self.layout.with_untracked(|l| l.tiles.iter().position(|t| t.key == key))
            && self.layout.with_untracked(|l| l.focus != i)
        {
            self.layout.update(|l| l.focus = i);
        }
    }

    /// Marks a tile the last used (in a tall column, it takes half the
    /// height).
    fn grow(&self, key: u64) {
        let latest = self.layout.with_untracked(|l| l.tile(key).is_none_or(|t| t.used == l.next));
        if !latest && let Some(i) = self.layout.with_untracked(|l| l.tiles.iter().position(|t| t.key == key)) {
            self.layout.update(|l| l.touch(i));
        }
    }

    pub fn set_active(&self, tile: u64, col: u64) {
        if self.layout.with_untracked(|l| l.tile(tile).is_some_and(|t| t.active != col)) {
            self.layout.update(|l| {
                if let Some(t) = l.tile_mut(tile) {
                    t.active = col;
                }
            });
        }
    }

    pub fn toggle_pin(&self, id: &str) {
        self.pinned.update(|p| match p.iter().position(|x| x == id) {
            Some(i) => {
                p.remove(i);
            }
            None => p.push(id.to_owned()),
        });
    }

    pub fn dismiss(&self, id: &str) {
        self.attention.update(|a| a.retain(|x| x != id));
    }

    /// Flags sessions that finish or fail while not shown, and clears the
    /// flag once they are.
    pub fn watch(&self, live: LiveFeed) {
        let ws = *self;
        let previous = StoredValue::new(HashMap::<String, SessionStatus>::new());
        Effect::new(move || {
            let sessions = live.sessions.get();
            let visible = ws.layout.with_untracked(Layout::visible_sessions);
            let mut flagged = Vec::new();
            previous.update_value(|prev| {
                for s in &sessions {
                    let was = prev.insert(s.id.clone(), s.status);
                    let ended = was == Some(SessionStatus::Running) && matches!(s.status, SessionStatus::Idle | SessionStatus::Failed);
                    if ended && !visible.contains(&s.id) {
                        flagged.push(s.id.clone());
                    }
                }
            });
            if !flagged.is_empty() {
                ws.attention.update(|a| {
                    for id in flagged {
                        a.retain(|x| x != &id);
                        a.insert(0, id);
                    }
                    // The latest ones; older ones you've let go.
                    a.truncate(MAX_ATTENTION);
                });
            }
        });
        Effect::new(move || {
            let visible = ws.layout.with(Layout::visible_sessions);
            if ws.attention.with_untracked(|a| a.iter().any(|id| visible.contains(id))) {
                ws.attention.update(|a| a.retain(|id| !visible.contains(id)));
            }
        });
    }

    /// Sessions for the dock and mission control: pinned, running, then
    /// needing attention (each once).
    pub fn tracked(&self, live: &LiveFeed) -> Vec<(Session, Tracked)> {
        let sessions = live.sessions.get();
        let find = |id: &str| sessions.iter().find(|s| s.id == id).cloned();
        let mut out: Vec<(Session, Tracked)> = Vec::new();
        let push = |s: Session, how: Tracked, out: &mut Vec<(Session, Tracked)>| {
            if !out.iter().any(|(x, _)| x.id == s.id) {
                out.push((s, how));
            }
        };
        // Waiting on you comes first.
        for s in sessions.iter().filter(|s| s.awaiting_approval) {
            push(s.clone(), Tracked::Approval, &mut out);
        }
        for id in self.pinned.get() {
            if let Some(s) = find(&id) {
                push(s, Tracked::Pinned, &mut out);
            }
        }
        for s in sessions.iter().filter(|s| s.status == SessionStatus::Running) {
            push(s.clone(), Tracked::Running, &mut out);
        }
        for id in self.attention.get() {
            if let Some(s) = find(&id) {
                push(s, Tracked::Attention, &mut out);
            }
        }
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tracked {
    /// A tool call waits for your approval.
    Approval,
    Pinned,
    Running,
    Attention,
}

pub fn use_workspace() -> Workspace {
    expect_context::<Workspace>()
}

/// The column a view is in.
#[derive(Clone, Copy)]
pub struct ColumnCtx {
    pub ws: Workspace,
    pub tile: u64,
    pub col: u64,
    /// Shown (the tile's current column, or the one beside it).
    pub visible: Signal<bool>,
    /// The column keyboard shortcuts act on.
    pub active: Signal<bool>,
    /// Scroll position, kept while the column is folded away, and whether
    /// it was at the end (then it comes back at the end, however much was
    /// added meanwhile).
    scroll: StoredValue<(i32, bool)>,
}

impl ColumnCtx {
    /// The element id of this column's root, for scoped DOM lookups.
    pub fn root_id(&self) -> String {
        format!("col-{}", self.col)
    }

    pub fn open(&self, view: View, how: How) {
        self.ws.open(Some((self.tile, self.col)), view, how);
    }

    pub fn replace(&self, view: View) {
        self.ws.replace(self.tile, self.col, view);
    }

    pub fn close(&self) {
        self.ws.close_column(self.tile, self.col);
    }
}

pub fn use_column() -> Option<ColumnCtx> {
    use_context::<ColumnCtx>()
}

/// Opens a view: from inside a column, beside it (or with `own`, in a tile
/// of its own); from elsewhere (the header's menus), in a tile of its own. ⌘/Ctrl-click opens
/// a new tile; middle-click and "copy link" use the plain URL.
#[component]
pub fn Go(
    to: View,
    #[prop(optional, into)] class: String,
    #[prop(optional, into)] title: String,
    #[prop(optional)] stop: bool,
    #[prop(optional)] own: bool,
    #[prop(optional)] brief_request: Option<i64>,
    children: Children,
) -> impl IntoView {
    let ws = use_workspace();
    let col = use_column();
    let href = brief_request.map_or_else(|| to.href(), |id| crate::place::href(&crate::brief::request_path(id)));
    let to = StoredValue::new(to);
    view! {
        <a href=href class=class title=title on:click=move |ev: web_sys::MouseEvent| {
            if ev.button() != 0 || ev.shift_key() || ev.alt_key() {
                return;
            }
            ev.prevent_default();
            if stop {
                ev.stop_propagation();
            }
            // The header's menus (outside any tile) open a tile of their
            // own too, so what you were looking at stays (docs/work.md §7.1).
            let how = match How::from_mouse(&ev) {
                How::Beside if own || col.is_none() => How::OwnTile,
                how => how,
            };
            match col {
                Some(c) => c.open(to.get_value(), how),
                None => ws.open(None, to.get_value(), how),
            }
            if let Some(id) = brief_request {
                ws.brief_request.set(Some(id));
            }
        }>{children()}</a>
    }
}

/// A scrolling area that remembers its position while its column is folded
/// away: where it was, or the end if it was at the end.
///
/// With `follow`, it keeps to the end as its content grows, while shown
/// again after being hidden, and as content renders late, until you
/// scroll away from the end yourself (wheel, touch, the scroll bar or
/// keys); back at the end, it follows again.
#[component]
pub fn ScrollBox(
    #[prop(optional)] node_ref: Option<NodeRef<Div>>,
    #[prop(optional, into)] class: String,
    #[prop(optional)] on_scroll: Option<Callback<()>>,
    #[prop(optional)] follow: Option<RwSignal<bool>>,
    children: Children,
) -> impl IntoView {
    let col = use_column();
    let node = node_ref.unwrap_or_default();
    if let Some(c) = col {
        Effect::new(move || {
            if c.visible.get() {
                let (top, end) = c.scroll.get_value();
                request_animation_frame(move || {
                    if let Some(el) = node.get_untracked() {
                        if end || follow.is_some_and(|f| f.get_untracked()) {
                            el.set_scroll_top(el.scroll_height());
                        } else if top > 0 {
                            el.set_scroll_top(top);
                        }
                    }
                });
            }
        });
    }
    // You moved it (rather than content arriving, or it being hidden).
    let by_you = StoredValue::new(false);
    if let Some(follow) = follow {
        let observer = StoredValue::new_local(None::<web_sys::ResizeObserver>);
        Effect::new(move || {
            let Some(el) = node.get() else { return };
            let to_end = wasm_bindgen::closure::Closure::<dyn Fn(js_sys::Array)>::new(move |_: js_sys::Array| {
                if let Some(el) = node.get_untracked()
                    && follow.get_untracked()
                    && el.client_height() > 0
                {
                    el.set_scroll_top(el.scroll_height());
                }
            });
            if let Ok(obs) = web_sys::ResizeObserver::new(to_end.as_ref().unchecked_ref()) {
                obs.observe(&el);
                // The content: it grows as events arrive and render.
                let children = el.children();
                for i in 0..children.length() {
                    if let Some(child) = children.item(i) {
                        obs.observe(&child);
                    }
                }
                observer.set_value(Some(obs));
            }
            to_end.forget();
        });
        on_cleanup(move || {
            if let Some(o) = observer.try_get_value().flatten() {
                o.disconnect();
            }
        });
    }
    let yours = move || by_you.set_value(true);
    view! {
        <div node_ref=node class=format!("min-h-0 flex-1 overflow-y-auto {class}")
            on:wheel=move |ev: web_sys::WheelEvent| if ev.delta_y() < 0.0 { yours() }
            on:touchstart=move |_| yours()
            on:keydown=move |ev: web_sys::KeyboardEvent| if matches!(ev.key().as_str(), "ArrowUp" | "PageUp" | "Home" | " ") { yours() }
            // The scroll bar: a press on the box itself, not its content.
            on:pointerdown=move |ev: web_sys::PointerEvent| {
                if ev.target().is_some_and(|t| node.get_untracked().is_some_and(|el| AsRef::<web_sys::EventTarget>::as_ref(&el) == &t)) {
                    yours();
                }
            }
            on:scroll=move |_| {
            let Some(el) = node.get_untracked() else { return };
            // Hidden: nothing to remember.
            if el.client_height() == 0 {
                return;
            }
            let end = at_end(&el);
            if let Some(c) = col
                && c.visible.get_untracked()
            {
                c.scroll.set_value((el.scroll_top(), end));
            }
            if let Some(f) = follow {
                if end {
                    f.set(true);
                    by_you.set_value(false);
                } else if by_you.get_value() {
                    f.set(false);
                }
            }
            if let Some(f) = on_scroll {
                f.run(());
            }
        }>{children()}</div>
    }
}

/// Whether a scrolling area is at (or within a line or two of) its end.
pub fn at_end(el: &web_sys::Element) -> bool {
    el.scroll_height() - el.scroll_top() - el.client_height() < 120
}

/// Where a tile goes, as fractions of the workspace.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

/// Where each tile goes, given its column and when it was last used (in
/// tile order; columns numbered from 0 without gaps). The columns sit side
/// by side, evenly; a column's tiles stack in tile order. A column of two
/// splits evenly; in a column of three or more, the tile last used there
/// takes half the height and the others share the rest.
pub fn arrange(tiles: &[(usize, u64)]) -> Vec<Rect> {
    let n = tiles.len();
    let cols = tiles.iter().map(|t| t.0 + 1).max().unwrap_or(1);
    let mut out = vec![Rect::default(); n];
    for c in 0..cols {
        let members: Vec<usize> = (0..n).filter(|&i| tiles[i].0 == c).collect();
        let k = members.len();
        let big = members.iter().copied().max_by_key(|&i| tiles[i].1);
        let weight = |i: usize| if k >= 3 && Some(i) == big { (k - 1) as f64 } else { 1.0 };
        let total: f64 = members.iter().map(|&i| weight(i)).sum();
        let mut top = 0.0;
        for &i in &members {
            let height = weight(i) / total;
            out[i] = Rect { left: c as f64 / cols as f64, top, width: 1.0 / cols as f64, height };
            top += height;
        }
    }
    out
}

/// The tiles. Keyboard: ⌘1–9 focus a tile, ⌘[ / ⌘] go back and forward in it.
#[component]
pub fn Tiles() -> impl IntoView {
    let ws = use_workspace();
    let _keys = window_event_listener(leptos::ev::keydown, move |ev| {
        if !(ev.meta_key() || ev.ctrl_key()) || ev.alt_key() {
            return;
        }
        match ev.key().as_str() {
            k @ ("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9") => {
                ev.prevent_default();
                ws.focus_tile(k.parse::<usize>().unwrap() - 1);
            }
            "[" => {
                ev.prevent_default();
                ws.step(-1);
            }
            "]" => {
                ev.prevent_default();
                ws.step(1);
            }
            _ => {}
        }
    });
    let keys = Memo::new(move |_| ws.layout.with(|l| l.tiles.iter().map(|t| t.key).collect::<Vec<_>>()));
    let rects = Memo::new(move |_| {
        ws.layout.with(|l| {
            let placed: Vec<(usize, u64)> = l.tiles.iter().map(|t| (t.stack.unwrap_or(0), t.used)).collect();
            l.tiles.iter().map(|t| t.key).zip(arrange(&placed)).collect::<HashMap<u64, Rect>>()
        })
    });
    // Tiles are cards with room between them, placed by `arrange` (so one
    // moving to another column keeps what it shows); on a phone, one fills
    // the screen.
    view! {
        <div class="flex min-h-0 min-w-0 flex-1 bg-zinc-100 dark:bg-black md:p-1">
            <div class="relative flex min-h-0 min-w-0 flex-1">
                <For each=move || keys.get() key=|k| *k let:key>
                    <TileView key rect=Signal::derive(move || rects.with(|r| r.get(&key).copied().unwrap_or_default())) />
                </For>
            </div>
        </div>
    }
}

/// A session's state, as its tile's bar shows it.
fn status_pill(s: &Session) -> (&'static str, &'static str) {
    if s.awaiting_approval {
        return ("bg-amber-500 text-white", "needs approval");
    }
    match s.status {
        SessionStatus::Running => ("bg-blue-600 text-white", "running"),
        SessionStatus::Idle => ("bg-white/70 text-zinc-600 dark:bg-black/30 dark:text-zinc-300", "idle"),
        SessionStatus::Starting => ("bg-white/70 text-amber-700 dark:bg-black/30 dark:text-amber-300", "starting"),
        SessionStatus::Failed => ("bg-red-600 text-white", "failed"),
    }
}

/// Where a popover opens: below the element clicked, kept on screen.
pub fn below(ev: &web_sys::MouseEvent, width: f64) -> (f64, f64) {
    let rect = ev
        .current_target()
        .and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok())
        .map(|el| el.get_bounding_client_rect());
    let vw = window().inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1200.0);
    match rect {
        Some(r) => ((r.left()).min(vw - width - 8.0).max(8.0), r.bottom() + 4.0),
        None => (8.0, 48.0),
    }
}

/// A popover at a place on screen (from [`below`]); a click outside it
/// closes it, and so does Esc.
#[component]
pub fn Popover(at: RwSignal<Option<(f64, f64)>>, #[prop(into)] class: String, children: ChildrenFn) -> impl IntoView {
    let class = StoredValue::new(class);
    let children = StoredValue::new(children);
    move || {
        at.get().map(|(x, y)| {
            view! {
                <div class="fixed inset-0 z-40" on:click=move |ev| { ev.stop_propagation(); at.set(None) }></div>
                <div
                    class=format!("fixed z-50 max-h-[75vh] overflow-y-auto rounded-lg border border-zinc-200 bg-white p-2 text-sm shadow-xl dark:border-zinc-700 dark:bg-zinc-900 {}", class.get_value())
                    style=format!("left:{x}px;top:{y}px")
                    on:keydown=move |ev| if ev.key() == "Escape" { ev.stop_propagation(); at.set(None) }
                >
                    {children.with_value(|c| c())}
                </div>
            }
        })
    }
}

#[component]
fn TileView(key: u64, rect: Signal<Rect>) -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    let focused = Memo::new(move |_| ws.layout.with(|l| l.tiles.get(l.focus).map(|t| t.key) == Some(key)));
    let many = Memo::new(move |_| ws.layout.with(|l| l.tiles.len() > 1));
    let tile = Memo::new(move |_| ws.layout.with(|l| l.tile(key).cloned()));
    let cols = Memo::new(move |_| tile.get().map(|t| t.columns.iter().map(|c| c.key).collect::<Vec<_>>()).unwrap_or_default());
    // The tile's name: what it was opened on, and the item open in it.
    let title = Signal::derive(move || {
        tile.get().map(|t| t.columns.iter().map(|c| c.view.label(&live)).collect::<Vec<_>>().join("  ›  ")).unwrap_or_default()
    });
    let can_back = move || tile.get().is_some_and(|t| !t.back.is_empty());
    let can_forward = move || tile.get().is_some_and(|t| !t.forward.is_empty());
    let two = move || tile.get().is_some_and(|t| t.columns.len() == 2);
    // The session the tile shows on its own (beside a list, its pane pins it).
    let pinnable = move || match tile.get().map(|t| t.columns) {
        Some(c) if c.len() == 1 => match &c[0].view {
            View::Session { id, .. } => Some(id.clone()),
            _ => None,
        },
        _ => None,
    };
    // The session it shows (alone, or as the item beside a list): its pane,
    // and how it is now. The bar wears its colour and says its state.
    let shown = Memo::new(move |_| {
        tile.get().and_then(|t| match t.columns.last().map(|c| (c.key, &c.view)) {
            Some((col, View::Session { id, .. })) => Some((col, id.clone())),
            _ => None,
        })
    });
    let session = Memo::new(move |_| shown.get().and_then(|(_, id)| live.sessions.with(|ss| ss.iter().find(|s| s.id == id).cloned())));
    let colour = Memo::new(move |_| session.get().map(|s| crate::colour::of(&s)));
    let palette_at = RwSignal::new(None::<(f64, f64)>);
    let details_at = RwSignal::new(None::<(f64, f64)>);
    let rename_at = RwSignal::new(None::<(f64, f64)>);
    let chrome = move || shown.get().and_then(|(col, _)| ws.chrome.with(|m| m.get(&col).copied()));
    let btn = "rounded px-1.5 py-0.5 text-zinc-500 hover:bg-black/10 hover:text-zinc-900 disabled:opacity-30 disabled:hover:bg-transparent dark:hover:bg-white/10 dark:hover:text-zinc-100";
    // Another tile dragged over this one: where it would go, by where the
    // pointer is (the outer fifths make a new column beside this one's).
    let section = NodeRef::<leptos::html::Section>::new();
    let zone = move |ev: &web_sys::DragEvent| -> Option<Drop> {
        let r = section.get_untracked()?.get_bounding_client_rect();
        let x = (f64::from(ev.client_x()) - r.left()) / r.width().max(1.0);
        let y = (f64::from(ev.client_y()) - r.top()) / r.height().max(1.0);
        Some(match () {
            _ if x < 0.2 => Drop::LeftOf(key),
            _ if x > 0.8 => Drop::RightOf(key),
            _ if y < 0.5 => Drop::Above(key),
            _ => Drop::Below(key),
        })
    };
    let other = move || ws.dragging.get_untracked().filter(|&k| k != key);
    let hint = move || match ws.drop_at.get() {
        Some(Drop::Above(k)) if k == key => Some("inset-x-0 top-0 h-1/2"),
        Some(Drop::Below(k)) if k == key => Some("inset-x-0 bottom-0 h-1/2"),
        Some(Drop::LeftOf(k)) if k == key => Some("inset-y-0 left-0 w-1/5"),
        Some(Drop::RightOf(k)) if k == key => Some("inset-y-0 right-0 w-1/5"),
        _ => None,
    };
    view! {
        <div
            class=move || format!(
                "min-h-0 min-w-0 flex-1 md:absolute md:left-(--l) md:top-(--t) md:h-(--h) md:w-(--w) md:flex md:p-1 md:transition-[left,top,width,height] md:duration-150 {}",
                if focused.get() { "flex" } else { "hidden" },
            )
            style=move || { let r = rect.get(); format!("--l:{:.4}%;--t:{:.4}%;--w:{:.4}%;--h:{:.4}%", r.left * 100.0, r.top * 100.0, r.width * 100.0, r.height * 100.0) }
        >
        <section
            class=move || format!(
                "relative flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden bg-white dark:bg-zinc-950 md:rounded-lg md:border md:shadow-sm {}",
                if focused.get() && many.get() { "md:border-blue-400 md:ring-1 md:ring-blue-400/40 dark:md:border-blue-500" } else { "md:border-zinc-200 dark:md:border-zinc-800" },
            )
            node_ref=section
            on:pointerdown=move |_| ws.focus_tile_key(key)
            on:click=move |_| ws.grow(key)
            on:dragover=move |ev: web_sys::DragEvent| {
                if other().is_some() {
                    ev.prevent_default();
                    let z = zone(&ev);
                    if ws.drop_at.get_untracked() != z {
                        ws.drop_at.set(z);
                    }
                }
            }
            on:drop=move |ev: web_sys::DragEvent| {
                if let (Some(k), Some(z)) = (other(), zone(&ev)) {
                    ev.prevent_default();
                    ws.move_tile(k, z);
                }
                ws.dragging.set(None);
                ws.drop_at.set(None);
            }
        >
            {move || hint().map(|at| view! {
                <div class=format!("pointer-events-none absolute z-20 rounded-md border-2 border-blue-500 bg-blue-500/15 {at}")></div>
            })}
            // The tile's own bar: its history, its name (and a session's
            // colour and state), and what you can do to the whole tile.
            // Dragged by its bar to another place or column.
            <div class=move || format!(
                "flex h-9 shrink-0 cursor-grab items-center gap-1 border-b px-2 text-xs active:cursor-grabbing {}",
                crate::colour::bar(colour.get().unwrap_or_default()),
            )
                draggable=move || if many.get() { "true" } else { "false" }
                title="Drag to move this tile: onto another tile's top or bottom half, or its left or right edge for a new column"
                on:dragstart=move |ev: web_sys::DragEvent| {
                    if let Some(dt) = ev.data_transfer() {
                        let _ = dt.set_data("text/plain", &format!("tile {key}"));
                        dt.set_effect_allowed("move");
                    }
                    ws.dragging.set(Some(key));
                }
                on:dragend=move |_| {
                    ws.dragging.set(None);
                    ws.drop_at.set(None);
                }
            >
                <button class=btn title="Back (⌘[)" prop:disabled=move || !can_back() on:click=move |_| { ws.focus_tile_key(key); ws.step(-1) }>"‹"</button>
                <button class=btn title="Forward (⌘])" prop:disabled=move || !can_forward() on:click=move |_| { ws.focus_tile_key(key); ws.step(1) }>"›"</button>
                {move || colour.get().map(|c| view! {
                    <button class=format!("ml-0.5 h-3 w-3 shrink-0 rounded-full ring-1 ring-black/10 hover:scale-125 dark:ring-white/20 {}", crate::colour::solid(c))
                        title="This session's colour: change it" on:click=move |ev| palette_at.set(Some(below(&ev, 180.0)))></button>
                })}
                <span class="min-w-0 flex-1 truncate px-1 font-medium text-zinc-700 dark:text-zinc-200" title=move || title.get()>{title}</span>
                {move || session.get().filter(|s| s.parent_id.is_none() && !s.observed).map(|_| view! {
                    <button class=btn title="Rename this session" on:click=move |ev| { ev.stop_propagation(); rename_at.set(Some(below(&ev, 320.0))) }>"✎"</button>
                })}
                {move || session.get().map(|s| {
                    let (cls, label) = status_pill(&s);
                    view! {
                        <span class=format!("flex shrink-0 items-center gap-1 rounded-full px-1.5 py-px text-[10px] font-medium {cls}")>
                            {(s.status == SessionStatus::Running).then(|| view! { <span class="h-1.5 w-1.5 animate-pulse rounded-full bg-white"></span> })}
                            {label}
                        </span>
                    }
                })}
                {move || chrome().is_some().then(|| view! {
                    <button class=btn title="Details: where it runs, labels, transcript, changes, files, terminal…" on:click=move |ev| details_at.set(Some(below(&ev, 560.0)))>"⋯"</button>
                })}
                {move || pinnable().map(|id| {
                    let (a, b) = (StoredValue::new(id.clone()), id.clone());
                    let on = move || ws.pinned.with(|p| p.contains(&a.get_value()));
                    view! {
                        <button class=btn title=move || if on() { "Unpin" } else { "Pin to the dock" } on:click=move |_| ws.toggle_pin(&b)>
                            {move || if on() { "★" } else { "☆" }}
                        </button>
                    }
                })}
                {move || two().then(|| view! {
                    <button class=format!("{btn} hidden md:inline") title="Move the open item to a tile of its own (or ⌘-click a link)" on:click=move |_| ws.detach(key)>"⧉"</button>
                })}
                <button class=btn title="Close this tile" on:click=move |_| ws.close_tile(key)>"✕"</button>
            </div>
            <Popover at=palette_at class="w-auto">
                {move || shown.get().map(|(_, id)| view! { <crate::colour::Palette session=id on_pick=move |()| palette_at.set(None) /> })}
            </Popover>
            <Popover at=rename_at class="w-[min(22rem,calc(100vw-1rem))] p-3">
                // Keyed on the id, and the name read untracked: the session changes as it works, and a rebuilt form loses what was typed.
                {move || shown.get().map(|(_, id)| {
                    let title = session.get_untracked().map(|s| s.title).unwrap_or_default();
                    view! { <crate::session::RenameForm session=id title on_done=move |()| rename_at.set(None) /> }
                })}
            </Popover>
            <Popover at=details_at class="w-[min(35rem,calc(100vw-1rem))] p-3">
                {move || chrome().map(|c| view! { <crate::session::SessionMeta chrome=c on_act=move |()| details_at.set(None) /> })}
            </Popover>
            <div class="flex min-h-0 flex-1">
                <For each=move || cols.get() key=|k| *k let:col>
                    <ColumnView tile=key col />
                </For>
            </div>
        </section>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brief_views_load_without_carrying_selection() {
        for json in [r#"{"view":"brief"}"#, r#"{"view":"brief","request":42}"#] {
            let view: View = serde_json::from_str(json).unwrap();
            assert_eq!(view, View::Brief);
            assert_eq!(view.path(), "/brief");
        }
    }

    #[test]
    fn opening_the_brief_reuses_its_mounted_column() {
        let mut layout = Layout::new(View::Mail);
        assert!(!layout.focus_brief());
        let key = layout.key();
        layout.tiles[0].columns.push(Column { key, view: View::Brief });
        layout.open_tile(View::Calendar);
        for _ in 0..2 {
            assert!(layout.focus_brief());
            assert_eq!(layout.tiles.len(), 2);
            assert_eq!(layout.focus, 0);
            assert_eq!(layout.tiles[0].cursor, 1);
            assert_eq!(layout.tiles[0].active, key);
            assert_eq!(layout.tiles[0].columns[1].key, key);
            assert_eq!(layout.tiles[0].columns[1].view, View::Brief);
        }
    }

    fn r(left: f64, top: f64, width: f64, height: f64) -> Rect {
        Rect { left, top, width, height }
    }

    /// Tiles placed as they open, as (column, when used) for `arrange`.
    fn opened(n: usize) -> Layout {
        let mut l = Layout::new(View::home());
        for _ in 1..n {
            l.open_tile(View::home());
        }
        l
    }

    fn placed(l: &Layout) -> Vec<(usize, u64)> {
        l.tiles.iter().map(|t| (t.stack.unwrap(), t.used)).collect()
    }

    fn stacks(l: &Layout) -> Vec<usize> {
        l.tiles.iter().map(|t| t.stack.unwrap()).collect()
    }

    #[test]
    fn up_to_four_tiles_sit_side_by_side() {
        let mut one = Layout::new(View::home());
        one.place();
        assert_eq!(arrange(&placed(&one)), vec![r(0.0, 0.0, 1.0, 1.0)]);
        assert_eq!(
            arrange(&placed(&opened(3))),
            vec![r(0.0, 0.0, 1.0 / 3.0, 1.0), r(1.0 / 3.0, 0.0, 1.0 / 3.0, 1.0), r(2.0 / 3.0, 0.0, 1.0 / 3.0, 1.0)]
        );
        assert!(arrange(&placed(&opened(4))).iter().all(|x| x.width == 0.25 && x.height == 1.0));
    }

    #[test]
    fn more_stack_in_the_columns_in_turn() {
        // The fifth halves the first column; the others stay full height.
        assert_eq!(stacks(&opened(9)), [0, 1, 2, 3, 0, 1, 2, 3, 0]);
        let five = arrange(&[(0, 1), (1, 2), (2, 3), (3, 4), (0, 5)]);
        assert_eq!((five[0], five[4]), (r(0.0, 0.0, 0.25, 0.5), r(0.0, 0.5, 0.25, 0.5)));
        assert!(five[1..4].iter().all(|x| x.height == 1.0));
        assert!(arrange(&placed(&opened(8))).iter().all(|x| x.height == 0.5));
    }

    #[test]
    fn in_a_tall_column_the_last_used_tile_takes_half() {
        // Nine: the first column is three high; tile 4 (in it) was used last.
        let used = [1, 2, 3, 4, 50, 6, 7, 8, 9];
        let tiles: Vec<(usize, u64)> = used.iter().enumerate().map(|(i, &u)| (i % 4, u)).collect();
        let nine = arrange(&tiles);
        assert_eq!((nine[0], nine[4], nine[8]), (r(0.0, 0.0, 0.25, 0.25), r(0.0, 0.25, 0.25, 0.5), r(0.0, 0.75, 0.25, 0.25)));
        assert!(nine[1..4].iter().chain(&nine[5..8]).all(|x| x.height == 0.5), "columns of two split evenly");
    }

    #[test]
    fn a_tile_dragged_to_another_column_or_a_new_one() {
        let mut l = opened(5);
        let keys: Vec<u64> = l.tiles.iter().map(|t| t.key).collect();
        l.focus = 2;
        // The fifth (under the first) dropped below the third: column 2.
        l.move_tile(keys[4], Drop::Below(keys[2]));
        assert_eq!(
            l.tiles.iter().map(|t| (t.key, t.stack.unwrap())).collect::<Vec<_>>(),
            [(keys[0], 0), (keys[1], 1), (keys[2], 2), (keys[4], 2), (keys[3], 3)]
        );
        assert_eq!(l.tiles[l.focus].key, keys[2], "focus stays on its tile");
        // Above the first: column 0 again, on top.
        l.move_tile(keys[4], Drop::Above(keys[0]));
        assert_eq!(l.tiles[0].key, keys[4]);
        assert_eq!(stacks(&l), [0, 0, 1, 2, 3]);
        // Four columns: no room for a fifth beside one.
        l.move_tile(keys[4], Drop::RightOf(keys[3]));
        assert_eq!(stacks(&l), [0, 0, 1, 2, 3], "unchanged");
        // Alone in its column, a tile moves its column; columns stay
        // numbered without gaps.
        l.move_tile(keys[1], Drop::LeftOf(keys[4]));
        assert_eq!(l.tile(keys[1]).unwrap().stack, Some(0));
        assert_eq!(l.tile(keys[4]).unwrap().stack, Some(1));
        assert_eq!(l.tile(keys[3]).unwrap().stack, Some(3));
        // Out of a column of two into a new one, when one is free.
        let mut l = opened(3);
        let k: Vec<u64> = l.tiles.iter().map(|t| t.key).collect();
        l.move_tile(k[2], Drop::Below(k[0]));
        assert_eq!(stacks(&l), [0, 0, 1]);
        l.move_tile(k[2], Drop::RightOf(k[1]));
        assert_eq!(l.tile(k[2]).unwrap().stack, Some(2));
        assert_eq!(arrange(&placed(&l)).iter().filter(|x| x.height == 1.0).count(), 3);
        // A tile closed leaves the others where they are.
        let mut l = opened(5);
        l.tiles.remove(1);
        l.place();
        assert_eq!(stacks(&l), [0, 1, 2, 0]);
    }
}

/// Where a pane is in its tile.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Pos {
    index: usize,
    cursor: usize,
    len: usize,
    /// The active pane of the focused tile.
    active: bool,
}

#[component]
fn ColumnView(tile: u64, col: u64) -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    let pos = Memo::new(move |_| {
        ws.layout.with(|l| {
            let ti = l.tiles.iter().position(|t| t.key == tile)?;
            let t = &l.tiles[ti];
            let index = t.columns.iter().position(|c| c.key == col)?;
            Some(Pos { index, cursor: t.cursor, len: t.columns.len(), active: t.active == col && l.focus == ti })
        })
    });
    let view =
        Memo::new(move |_| ws.layout.with(|l| l.tile(tile).and_then(|t| t.columns.iter().find(|c| c.key == col)).map(|c| c.view.clone())));
    let ctx = ColumnCtx {
        ws,
        tile,
        col,
        visible: Signal::derive(move || pos.get().is_some()),
        active: Signal::derive(move || pos.get().is_some_and(|p| p.active)),
        scroll: StoredValue::new((0, false)),
    };
    provide_context(ctx);
    let label = Signal::derive(move || view.get().map(|v| v.label(&live)).unwrap_or_default());
    view! {
        // The list beside an item is narrower; a phone shows one pane.
        <div
            id=ctx.root_id()
            class=move || match pos.get() {
                Some(p) if p.len == 2 && p.index == 0 => format!(
                    "min-w-0 flex-col border-r border-zinc-200 dark:border-zinc-800 md:flex md:basis-[40%] md:shrink-0 {}",
                    if p.cursor == 0 { "flex flex-1" } else { "hidden" },
                ),
                Some(p) => format!("min-w-0 flex-1 flex-col md:flex {}", if p.cursor == p.index { "flex" } else { "hidden" }),
                None => "hidden".to_owned(),
            }
            on:pointerdown=move |_| ws.set_active(tile, col)
        >
            // A pane has its own header only beside another; alone, the tile's bar names it.
            {move || pos.get().is_some_and(|p| p.len == 2).then(|| view! {
                <ColumnHeader label pos=Signal::derive(move || pos.get()) view=Signal::derive(move || view.get()) />
            })}
            <div class="relative flex min-h-0 flex-1 flex-col">
                {move || view.get().map(render_view)}
            </div>
        </div>
    }
}

/// A pane's slim header, shown when its tile has two panes: its name, pin
/// (sessions), and close (the tile's ✕ closes the tile). On a phone, the
/// item's header goes back to the list beside it.
#[component]
fn ColumnHeader(label: Signal<String>, pos: Signal<Option<Pos>>, view: Signal<Option<View>>) -> impl IntoView {
    let ctx = use_column().expect("in a pane");
    let ws = ctx.ws;
    let session = move || match view.get() {
        Some(View::Session { id, .. }) => Some(id),
        _ => None,
    };
    let pinned = move || session().is_some_and(|id| ws.pinned.with(|p| p.contains(&id)));
    let btn = "rounded px-1 text-zinc-400 hover:bg-zinc-200 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100";
    view! {
        <div class=move || format!(
            "flex h-7 shrink-0 items-center gap-1 border-b border-zinc-100 px-2 text-[11px] dark:border-zinc-900 {}",
            if ctx.active.get() { "text-zinc-700 dark:text-zinc-300" } else { "text-zinc-500" },
        )>
            {move || pos.get().filter(|p| p.index == 1).map(|_| view! {
                <button class=format!("{btn} md:hidden") title="Back to the list" on:click=move |_| ws.show_list(ctx.tile)>"‹"</button>
            })}
            <span class="min-w-0 flex-1 truncate">{label}</span>
            {move || session().map(|id| view! {
                <button class=btn title=move || if pinned() { "Unpin" } else { "Pin to the dock" } on:click=move |_| ws.toggle_pin(&id)>
                    {move || if pinned() { "★" } else { "☆" }}
                </button>
            })}
            {move || pos.get().filter(|p| p.len == 2).map(|_| view! {
                <button class=btn title="Close this pane" on:click=move |_| ctx.close()>"×"</button>
            })}
        </div>
    }
}

fn render_view(view: View) -> AnyView {
    match view {
        View::Session { id, target } => view! { <crate::session::SessionView id target /> }.into_any(),
        other => view! {
            <ScrollBox>
                {match other {
                    View::Sessions => view! { <crate::sessions::SessionsPage /> }.into_any(),
                    View::Search { q, kind, worker } => view! { <crate::search::SearchPage q kind worker /> }.into_any(),
                    View::Workers => view! { <crate::fleet::FleetPage /> }.into_any(),
                    View::Worker { id } => view! { <crate::fleet::WorkerPage id /> }.into_any(),
                    View::Settings { note, section } => view! { <crate::settings::SettingsPage note section /> }.into_any(),
                    View::Inbox => view! { <crate::activity::InboxPage /> }.into_any(),
                    View::Mission => view! { <crate::activity::MissionPage /> }.into_any(),
                    View::Usage => view! { <crate::usage::UsagePage /> }.into_any(),
                    View::Analytics => view! { <crate::analytics::AnalyticsPage /> }.into_any(),
                    View::Activity => view! { <crate::monitor::ActivityPage /> }.into_any(),
                    View::Memex => view! { <crate::memex::MemexPage /> }.into_any(),
                    // Each realm has its own mail, calendars and contacts:
                    // drawn again for the realm you pick.
                    View::Mail => (move || { crate::api::realm().track(); view! { <crate::mail::MailPage /> } }).into_any(),
                    View::Chat => view! { <crate::chat::ChatPage /> }.into_any(),
                    View::Brief => (move || { crate::api::realm().track(); view! { <crate::brief::BriefPage /> } }).into_any(),
                    View::Calendar => (move || { crate::api::realm().track(); view! { <crate::calendar::CalendarPage /> } }).into_any(),
                    View::Contacts => (move || { crate::api::realm().track(); view! { <crate::contacts::ContactsPage /> } }).into_any(),
                    View::Photos => view! { <crate::photos::PhotosPage /> }.into_any(),
                    View::Decisions => view! { <crate::decisions::DecisionsPage /> }.into_any(),
                    View::Work { id } if id.is_empty() => view! { <crate::work::WorkPage /> }.into_any(),
                    View::Work { id } => view! { <crate::work::WorkItemPage id=id.clone() /> }.into_any(),
                    View::Collections { name } => view! { <crate::collections::CollectionsPage name=name.clone() /> }.into_any(),
                    View::App { name, record } => view! { <crate::views::ViewPage name=name.clone() record=record.clone() /> }.into_any(),
                    View::Realms => view! { <crate::explorer::RealmsMap /> }.into_any(),
                    View::Explore { realm, section, kind, id } => view! { <crate::explorer::ExplorePage realm=realm.clone() section=section.clone() kind=kind.clone() id=id.clone() /> }.into_any(),
                    View::Session { .. } => ().into_any(),
                }}
            </ScrollBox>
        }
        .into_any(),
    }
}

/// A path segment, percent-encoded.
pub fn encode(s: &str) -> String {
    js_sys::encode_uri_component(s).as_string().unwrap_or_default()
}

fn decode(s: &str) -> String {
    js_sys::decode_uri_component(&s.replace('+', " ")).ok().and_then(|v| v.as_string()).unwrap_or_else(|| s.to_owned())
}
