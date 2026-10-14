//! The workspace: up to four tiles side by side, each a stack of columns.
//!
//! Opening something from a column puts it in a new column to the right
//! (closing the columns after it), so where you came from stays open: a
//! search stays beside the session you opened from it. Earlier columns fold
//! into spines; a spine opens again on click, keeping the columns after it
//! until you open something new. ⌘/Ctrl-click opens in a new tile instead.
//!
//! On a phone one tile shows at a time (a switcher at the bottom) and only
//! its current column, with back and forward in the column header.
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
        let get = |k: &str| query.get(k).cloned().unwrap_or_default();
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
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
        }
    }

    /// Lists of sessions: opening a second session from one opens a tile.
    fn is_list(&self) -> bool {
        matches!(self, View::Sessions | View::Search { .. } | View::Inbox | View::Mission)
    }

    /// A top-level section (not one session or worker): the header's nav
    /// and search swap one section for another in place instead of
    /// stacking columns.
    fn is_section(&self) -> bool {
        !matches!(self, View::Session { .. } | View::Worker { .. }) && !matches!(self, View::Collections { name } if !name.is_empty())
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
    pub columns: Vec<Column>,
    /// The column the tile is showing (the one before it shows beside it
    /// when there is room); columns after it are folded on the right.
    pub cursor: usize,
    /// The column last interacted with: keyboard shortcuts act on it.
    pub active: u64,
    /// When the tile was last focused (a counter), to pick the least
    /// recently used one to replace when all tiles are in use.
    #[serde(default)]
    pub used: u64,
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
        Tile { key: tk, columns: vec![Column { key: ck, view }], cursor: 0, active: ck, used: self.next }
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
            let t = &mut self.tiles[lru];
            t.columns = vec![Column { key, view }];
            t.cursor = 0;
            t.active = key;
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
            && self.tiles.iter().all(|t| !t.columns.is_empty() && t.cursor < t.columns.len())
    }

    /// Session ids shown now (a tile's current column and the one beside it).
    pub fn visible_sessions(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        for t in &self.tiles {
            for i in t.cursor.saturating_sub(1)..=t.cursor {
                if let Some(Column { view: View::Session { id, .. }, .. }) = t.columns.get(i) {
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
    /// In a new column right of where it was clicked.
    Beside,
    /// In a new tile (⌘/Ctrl-click).
    NewTile,
}

impl How {
    pub fn from_mouse(ev: &web_sys::MouseEvent) -> Self {
        if ev.meta_key() || ev.ctrl_key() { How::NewTile } else { How::Beside }
    }
}

const LAYOUT_KEY: &str = "c6.workspace.v1";
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

    /// Opens `view` from column `from` (tile key, column key), or from the
    /// focused tile's current column.
    ///
    /// A session already open somewhere is shown where it is. A session
    /// opened from a list that already has something beside it gets a new
    /// tile, so sessions open side by side instead of replacing each other.
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
            if how == How::NewTile && l.tiles.len() < MAX_TILES {
                l.open_tile(origin, view);
                return;
            }
            let from_list = from
                .and_then(|(t, c)| l.tile(t)?.columns.iter().position(|x| x.key == c).map(|i| (t, i)))
                .filter(|&(t, i)| l.tile(t).is_some_and(|t| t.columns[i].view.is_list() && t.columns.len() > i + 1));
            if matches!(view, View::Session { .. }) && from_list.is_some() {
                l.open_tile(origin, view);
                return;
            }
            let ti = origin;
            let key = l.key();
            let t = &mut l.tiles[ti];
            let at = from.and_then(|(_, c)| t.columns.iter().position(|x| x.key == c)).unwrap_or(t.cursor);
            if from.is_none() && t.columns[at].view.is_section() && view.is_section() {
                // From the header (nav, search box): swap the section.
                t.columns[at].view = view;
                t.columns.truncate(at + 1);
                t.cursor = at;
                t.active = t.columns[at].key;
            } else if t.columns.get(at + 1).is_some_and(|c| c.view == view) {
                // Already open next to it: just show it.
                t.cursor = at + 1;
                t.active = t.columns[at + 1].key;
            } else {
                t.columns.truncate(at + 1);
                t.columns.push(Column { key, view });
                t.cursor = t.columns.len() - 1;
                t.active = key;
            }
            l.focus = ti;
            l.touch(ti);
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

    pub fn close_column(&self, tile: u64, col: u64) {
        self.layout.update(|l| {
            let Some(ti) = l.tiles.iter().position(|t| t.key == tile) else { return };
            let t = &mut l.tiles[ti];
            let Some(i) = t.columns.iter().position(|c| c.key == col) else { return };
            t.columns.remove(i);
            if t.columns.is_empty() {
                if l.tiles.len() == 1 {
                    *l = Layout::new(View::Sessions);
                } else {
                    l.tiles.remove(ti);
                    l.focus = l.focus.min(l.tiles.len() - 1);
                }
                return;
            }
            if i <= t.cursor && t.cursor > 0 {
                t.cursor -= 1;
            }
            t.cursor = t.cursor.min(t.columns.len() - 1);
            t.active = t.columns[t.cursor].key;
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

    /// Shows column `col` (from a spine), keeping the columns after it.
    pub fn show(&self, tile: u64, col: u64) {
        self.layout.update(|l| {
            if let Some(t) = l.tile_mut(tile)
                && let Some(i) = t.columns.iter().position(|c| c.key == col)
            {
                t.cursor = i;
                t.active = col;
            }
        });
    }

    /// Moves the focused tile's view back (left) or forward (right).
    pub fn step(&self, delta: isize) {
        self.layout.update(|l| {
            let t = &mut l.tiles[l.focus];
            let next = t.cursor as isize + delta;
            if next >= 0 && (next as usize) < t.columns.len() {
                t.cursor = next as usize;
                t.active = t.columns[t.cursor].key;
            }
        });
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

/// The tiles. Keyboard: ⌘1–4 focus a tile, ⌘[ / ⌘] step back and forward.
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
    view! {
        <div class="flex min-h-0 min-w-0 flex-1">
            <For each=move || keys.get() key=|k| *k let:key>
                <TileView key />
            </For>
        </div>
    }
}

#[component]
fn TileView(key: u64) -> impl IntoView {
    let ws = use_workspace();
    let focused = Memo::new(move |_| ws.layout.with(|l| l.tiles.get(l.focus).map(|t| t.key) == Some(key)));
    let many = Memo::new(move |_| ws.layout.with(|l| l.tiles.len() > 1));
    let cols = Memo::new(move |_| {
        ws.layout.with(|l| l.tile(key).map(|t| t.columns.iter().map(|c| c.key).collect::<Vec<_>>()).unwrap_or_default())
    });
    view! {
        <section
            class=move || format!(
                "@container relative min-w-0 flex-1 flex-col border-zinc-200 dark:border-zinc-800 md:flex md:border-l md:first:border-l-0 {} {}",
                if focused.get() { "flex" } else { "hidden" },
                if focused.get() && many.get() { "md:shadow-[inset_0_2px_0_0_rgb(59_130_246)]" } else { "" },
            )
            on:pointerdown=move |_| ws.focus_tile_key(key)
        >
            <div class="flex min-h-0 flex-1">
                <For each=move || cols.get() key=|k| *k let:col>
                    <ColumnView tile=key col />
                </For>
            </div>
        </section>
    }
}

/// Where a column is in its tile.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Pos {
    index: usize,
    cursor: usize,
    len: usize,
    /// The active column of the focused tile.
    active: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    /// The tile's current column.
    Main,
    /// Just before it: shown beside it when the tile is wide.
    Prev,
    /// Folded away to the left.
    Folded,
    /// Folded away to the right (after the current column).
    After,
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
    let role = Memo::new(move |_| match pos.get() {
        Some(p) if p.index == p.cursor => Role::Main,
        Some(p) if p.index + 1 == p.cursor => Role::Prev,
        Some(p) if p.index > p.cursor => Role::After,
        _ => Role::Folded,
    });
    let ctx = ColumnCtx {
        ws,
        tile,
        col,
        visible: Signal::derive(move || matches!(role.get(), Role::Main | Role::Prev)),
        active: Signal::derive(move || pos.get().is_some_and(|p| p.active)),
        scroll: StoredValue::new(0),
    };
    provide_context(ctx);
    let label = Signal::derive(move || view.get().map(|v| v.label(&live)).unwrap_or_default());
    let spine_class = "hidden w-9 shrink-0 cursor-pointer items-start justify-center overflow-hidden border-zinc-200 bg-zinc-100/70 py-3 text-xs text-zinc-500 hover:bg-zinc-200/70 hover:text-zinc-900 dark:border-zinc-800 dark:bg-zinc-900/60 dark:hover:bg-zinc-800 dark:hover:text-zinc-100";
    view! {
        <div class=move || match role.get() {
            Role::Main => "flex min-w-0 flex-1",
            Role::Prev => "flex shrink-0 @min-[60rem]:min-w-0 @min-[60rem]:basis-[42%] @min-[60rem]:border-r @min-[60rem]:border-zinc-200 dark:@min-[60rem]:border-zinc-800",
            Role::Folded | Role::After => "flex shrink-0",
        }>
            // Folded columns show as spines (desktop only).
            <button
                class=move || match role.get() {
                    Role::Main => "hidden".to_owned(),
                    Role::Prev => format!("{spine_class} border-r md:flex @min-[60rem]:!hidden"),
                    Role::Folded => format!("{spine_class} border-r md:flex"),
                    Role::After => format!("{spine_class} border-l md:flex"),
                }
                title=move || format!("Show {}", label.get())
                on:click=move |_| ws.show(tile, col)
            >
                <span class="max-h-[60vh] truncate [writing-mode:vertical-rl]">{label}</span>
            </button>
            <div
                id=ctx.root_id()
                class=move || match role.get() {
                    Role::Main => "flex min-w-0 flex-1 flex-col",
                    Role::Prev => "hidden min-w-0 flex-1 flex-col @min-[60rem]:flex",
                    Role::Folded | Role::After => "hidden",
                }
                on:pointerdown=move |_| ws.set_active(tile, col)
            >
                <ColumnHeader label pos=Signal::derive(move || pos.get()) view=Signal::derive(move || view.get()) />
                <div class="relative flex min-h-0 flex-1 flex-col">
                    {move || view.get().map(render_view)}
                </div>
            </div>
        </div>
    }
}

/// Back/forward (phones), the column's name, pin, new tile and close.
#[component]
fn ColumnHeader(label: Signal<String>, pos: Signal<Option<Pos>>, view: Signal<Option<View>>) -> impl IntoView {
    let ctx = use_column().expect("in a column");
    let ws = ctx.ws;
    let session = move || match view.get() {
        Some(View::Session { id, .. }) => Some(id),
        _ => None,
    };
    let pinned = move || session().is_some_and(|id| ws.pinned.with(|p| p.contains(&id)));
    let btn = "rounded px-1.5 py-0.5 text-zinc-500 hover:bg-zinc-200 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100";
    view! {
        <div class=move || format!(
            "flex h-8 shrink-0 items-center gap-1 border-b px-2 text-xs {}",
            if ctx.active.get() { "border-zinc-300 bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-900" } else { "border-zinc-200 bg-zinc-50 dark:border-zinc-800 dark:bg-zinc-950" },
        )>
            {move || pos.get().filter(|p| p.index > 0).map(|_| view! {
                <button class=format!("{btn} md:hidden") title="Back" on:click=move |_| ws.step(-1)>"‹"</button>
            })}
            <span class="min-w-0 flex-1 truncate font-medium text-zinc-700 dark:text-zinc-300">{label}</span>
            {move || pos.get().filter(|p| p.index + 1 < p.len).map(|_| view! {
                <button class=format!("{btn} md:hidden") title="Forward" on:click=move |_| ws.step(1)>"›"</button>
            })}
            {move || session().map(|id| view! {
                <button class=btn title=move || if pinned() { "Unpin" } else { "Pin to the dock" } on:click=move |_| ws.toggle_pin(&id)>
                    {move || if pinned() { "★" } else { "☆" }}
                </button>
            })}
            <button class=format!("{btn} hidden md:inline") title="Open in a new tile (or ⌘-click a link)" on:click=move |_| {
                if let Some(v) = view.get_untracked() {
                    ws.open(None, v, How::NewTile);
                }
            }>"⧉"</button>
            <button class=btn title="Close" on:click=move |_| ctx.close()>"✕"</button>
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
                    View::Mail => view! { <crate::mail::MailPage /> }.into_any(),
                    View::Calendar => view! { <crate::calendar::CalendarPage /> }.into_any(),
                    View::Contacts => view! { <crate::contacts::ContactsPage /> }.into_any(),
                    View::Photos => view! { <crate::photos::PhotosPage /> }.into_any(),
                    View::Decisions => view! { <crate::decisions::DecisionsPage /> }.into_any(),
                    View::Collections { name } => view! { <crate::collections::CollectionsPage name=name.clone() /> }.into_any(),
                    View::Session { .. } => ().into_any(),
                }}
            </ScrollBox>
        }
        .into_any(),
    }
}
