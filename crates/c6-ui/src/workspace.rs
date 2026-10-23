//! The workspace: up to four tiles side by side, each a framed card.
//!
//! A tile has at most two panes: a list (or whatever it was opened on) and
//! the item opened from it. One rule: a click inside a tile opens in that
//! tile's right pane; the header's navigation replaces the whole tile; only
//! ⌘/Ctrl-click (or a tile's ⧉) makes a new tile. Each tile keeps its own
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

use crate::api::{self, LiveFeed};
use crate::util::{storage_get, storage_set};

pub const MAX_TILES: usize = 4;

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
    Memex,
    Mail,
    Calendar,
    Contacts,
    Photos,
    Decisions,
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
}

fn enc(s: &str) -> String {
    js_sys::encode_uri_component(s).as_string().unwrap_or_default()
}

impl View {
    pub fn session(id: impl Into<String>) -> Self {
        View::Session { id: id.into(), target: None }
    }

    /// The URL for this view (shown in the address bar, and for copying).
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
            View::Memex => "/memex".into(),
            View::Mail => "/mail".into(),
            View::Calendar => "/calendar".into(),
            View::Contacts => "/contacts".into(),
            View::Photos => "/photos".into(),
            View::Decisions => "/decisions".into(),
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
        Self::parse(&path, &query, &hash)
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
            ["memex"] => View::Memex,
            ["mail"] => View::Mail,
            ["calendar"] => View::Calendar,
            ["contacts"] => View::Contacts,
            ["photos"] => View::Photos,
            ["decisions"] | ["todos"] => View::Decisions,
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
            _ => View::Sessions,
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
            View::Memex => "Memex".into(),
            View::Mail => "Mail".into(),
            View::Calendar => "Calendar".into(),
            View::Contacts => "Contacts".into(),
            View::Photos => "Photos".into(),
            View::Decisions => "Decisions".into(),
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

    /// Opens `view` in a new tile at the right end (left to right in the
    /// order opened), or, when all tiles are in use, in place of the least
    /// recently used tile other than `origin`.
    fn open_tile(&mut self, origin: usize, view: View) {
        if self.tiles.len() < MAX_TILES {
            let tile = self.new_tile(view);
            self.tiles.push(tile);
            self.focus = self.tiles.len() - 1;
        } else {
            let lru = (0..self.tiles.len()).filter(|&i| i != origin).min_by_key(|&i| self.tiles[i].used).unwrap_or(origin);
            let key = self.key();
            self.tiles[lru].go(vec![Column { key, view }]);
            self.focus = lru;
        }
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
            && self.tiles.len() <= MAX_TILES
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
    /// Sessions that finished or failed while not shown, newest first.
    pub attention: RwSignal<Vec<String>>,
    pub pinned: RwSignal<Vec<String>>,
}

fn load<T: for<'de> Deserialize<'de>>(key: &str) -> Option<T> {
    serde_json::from_str(&storage_get(key)?).ok()
}

impl Workspace {
    /// The saved workspace, with the page's URL opened in it.
    pub fn restore() -> Self {
        let here = View::from_location();
        let layout = match load::<Layout>(LAYOUT_KEY).filter(Layout::valid) {
            None => Layout::new(here.clone()),
            Some(l) => l,
        };
        let ws = Workspace {
            layout: RwSignal::new(layout),
            attention: RwSignal::new(load(ATTENTION_KEY).unwrap_or_default()),
            pinned: RwSignal::new(load(PINNED_KEY).unwrap_or_default()),
        };
        // A link or redirect to a particular view opens it (the home page
        // just restores the workspace).
        if here != View::Sessions && ws.layout.with_untracked(|l| l.focused_view() != Some(&here)) {
            ws.open(None, here, How::Beside);
        }
        // Save, and show the focused column in the address bar.
        Effect::new(move || {
            let l = ws.layout.get();
            storage_set(LAYOUT_KEY, &serde_json::to_string(&l).unwrap_or_default());
            if let Some(v) = l.focused_view() {
                let _ = window().history().and_then(|h| h.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&v.path())));
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
            if how == How::NewTile {
                l.open_tile(origin, view);
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
            if l.tiles[ti].columns.len() < 2 || l.tiles.len() >= MAX_TILES {
                return;
            }
            let item = l.tiles[ti].columns[1].view.clone();
            let list = l.tiles[ti].columns[0].clone();
            l.tiles[ti].go(vec![list]);
            l.open_tile(ti, item);
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
                *l = Layout::new(View::Sessions);
            } else {
                l.tiles.remove(ti);
                l.focus = l.focus.min(l.tiles.len() - 1);
            }
        });
    }

    pub fn close_tile(&self, tile: u64) {
        self.layout.update(|l| {
            let Some(ti) = l.tiles.iter().position(|t| t.key == tile) else { return };
            if l.tiles.len() == 1 {
                *l = Layout::new(View::Sessions);
            } else {
                l.tiles.remove(ti);
                l.focus = l.focus.min(l.tiles.len() - 1);
            }
        });
    }

    pub fn new_tile(&self) {
        self.open(None, View::Sessions, How::NewTile);
    }

    pub fn focus_tile(&self, index: usize) {
        self.layout.update(|l| {
            if index < l.tiles.len() && l.focus != index {
                l.focus = index;
                l.touch(index);
            }
        });
    }

    fn focus_tile_key(&self, key: u64) {
        if let Some(i) = self.layout.with_untracked(|l| l.tiles.iter().position(|t| t.key == key)) {
            self.focus_tile(i);
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
    /// Scroll position, kept while the column is folded away.
    scroll: StoredValue<i32>,
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

/// Opens a view: from inside a column, beside it; from elsewhere, in the
/// focused tile. ⌘/Ctrl-click opens a new tile; middle-click and "copy
/// link" use the plain URL.
#[component]
pub fn Go(
    to: View,
    #[prop(optional, into)] class: String,
    #[prop(optional, into)] title: String,
    #[prop(optional)] stop: bool,
    children: Children,
) -> impl IntoView {
    let ws = use_workspace();
    let col = use_column();
    let href = to.path();
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
            let how = How::from_mouse(&ev);
            match col {
                Some(c) => c.open(to.get_value(), how),
                None => ws.open(None, to.get_value(), how),
            }
        }>{children()}</a>
    }
}

/// A scrolling area that remembers its position while its column is folded
/// away.
#[component]
pub fn ScrollBox(
    #[prop(optional)] node_ref: Option<NodeRef<Div>>,
    #[prop(optional, into)] class: String,
    #[prop(optional)] on_scroll: Option<Callback<()>>,
    children: Children,
) -> impl IntoView {
    let col = use_column();
    let node = node_ref.unwrap_or_default();
    if let Some(c) = col {
        Effect::new(move || {
            if c.visible.get() {
                let top = c.scroll.get_value();
                request_animation_frame(move || {
                    if let Some(el) = node.get_untracked()
                        && top > 0
                    {
                        el.set_scroll_top(top);
                    }
                });
            }
        });
    }
    view! {
        <div node_ref=node class=format!("min-h-0 flex-1 overflow-y-auto {class}") on:scroll=move |_| {
            if let (Some(c), Some(el)) = (col, node.get_untracked())
                && c.visible.get_untracked()
            {
                c.scroll.set_value(el.scroll_top());
            }
            if let Some(f) = on_scroll {
                f.run(());
            }
        }>{children()}</div>
    }
}

/// The tiles. Keyboard: ⌘1–4 focus a tile, ⌘[ / ⌘] go back and forward in it.
#[component]
pub fn Tiles() -> impl IntoView {
    let ws = use_workspace();
    let _keys = window_event_listener(leptos::ev::keydown, move |ev| {
        if !(ev.meta_key() || ev.ctrl_key()) || ev.alt_key() {
            return;
        }
        match ev.key().as_str() {
            k @ ("1" | "2" | "3" | "4") => {
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
    // Tiles are cards with room between them; on a phone, one fills the screen.
    view! {
        <div class="flex min-h-0 min-w-0 flex-1 bg-zinc-100 dark:bg-black md:gap-2 md:p-2">
            <For each=move || keys.get() key=|k| *k let:key>
                <TileView key />
            </For>
        </div>
    }
}

#[component]
fn TileView(key: u64) -> impl IntoView {
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
    let btn = "rounded px-1.5 py-0.5 text-zinc-500 hover:bg-zinc-200 hover:text-zinc-900 disabled:opacity-30 disabled:hover:bg-transparent dark:hover:bg-zinc-800 dark:hover:text-zinc-100";
    view! {
        <section
            class=move || format!(
                "relative min-w-0 flex-1 flex-col overflow-hidden bg-white dark:bg-zinc-950 md:flex md:rounded-lg md:border md:shadow-sm {} {}",
                if focused.get() { "flex" } else { "hidden" },
                if focused.get() && many.get() { "md:border-blue-400 md:ring-1 md:ring-blue-400/40 dark:md:border-blue-500" } else { "md:border-zinc-200 dark:md:border-zinc-800" },
            )
            on:pointerdown=move |_| ws.focus_tile_key(key)
        >
            // The tile's own bar: its history, its name, and what you can do to the whole tile.
            <div class="flex h-9 shrink-0 items-center gap-1 border-b border-zinc-200 bg-zinc-50 px-2 text-xs dark:border-zinc-800 dark:bg-zinc-900">
                <button class=btn title="Back (⌘[)" prop:disabled=move || !can_back() on:click=move |_| { ws.focus_tile_key(key); ws.step(-1) }>"‹"</button>
                <button class=btn title="Forward (⌘])" prop:disabled=move || !can_forward() on:click=move |_| { ws.focus_tile_key(key); ws.step(1) }>"›"</button>
                <span class="min-w-0 flex-1 truncate px-1 font-medium text-zinc-700 dark:text-zinc-200">{title}</span>
                {move || pinnable().map(|id| {
                    let (a, b) = (StoredValue::new(id.clone()), id.clone());
                    let on = move || ws.pinned.with(|p| p.contains(&a.get_value()));
                    view! {
                        <button class=btn title=move || if on() { "Unpin" } else { "Pin to the dock" } on:click=move |_| ws.toggle_pin(&b)>
                            {move || if on() { "★" } else { "☆" }}
                        </button>
                    }
                })}
                {move || (two() && ws.layout.with(|l| l.tiles.len() < MAX_TILES)).then(|| view! {
                    <button class=format!("{btn} hidden md:inline") title="Move the open item to a tile of its own (or ⌘-click a link)" on:click=move |_| ws.detach(key)>"⧉"</button>
                })}
                <button class=btn title="Close this tile" on:click=move |_| ws.close_tile(key)>"✕"</button>
            </div>
            <div class="flex min-h-0 flex-1">
                <For each=move || cols.get() key=|k| *k let:col>
                    <ColumnView tile=key col />
                </For>
            </div>
        </section>
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
        scroll: StoredValue::new(0),
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
                    View::Memex => view! { <crate::memex::MemexPage /> }.into_any(),
                    // Each realm has its own mail, calendars and contacts:
                    // drawn again for the realm you pick.
                    View::Mail => (move || { crate::api::realm().track(); view! { <crate::mail::MailPage /> } }).into_any(),
                    View::Calendar => (move || { crate::api::realm().track(); view! { <crate::calendar::CalendarPage /> } }).into_any(),
                    View::Contacts => (move || { crate::api::realm().track(); view! { <crate::contacts::ContactsPage /> } }).into_any(),
                    View::Photos => view! { <crate::photos::PhotosPage /> }.into_any(),
                    View::Decisions => view! { <crate::decisions::DecisionsPage /> }.into_any(),
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
