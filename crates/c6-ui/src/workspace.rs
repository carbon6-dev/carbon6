//! The workspace: tiles, each a framed card. Up to four sit side by side;
//! more stack in those four columns (the fifth under the first, the sixth
//! under the second, …), and in a column of three or more the tile last
//! used there takes half its height (see [`arrange`]).
//!
//! A tile has at most two panes: a list (or whatever it was opened on) and
//! the item opened from it. One rule: a click inside a tile opens in that
//! tile's right pane; the header's navigation opens a tile of its own (the
//! one already showing it, else a new one). An existing destination is
//! focused in either pane; only ⌘/Ctrl-click makes another copy.
//! Each tile keeps its own
//! back and forward history (⌘[ and ⌘]).
//!
//! On a phone one tile shows at a time (a switcher at the bottom) and one
//! pane of it, with back in its header.
//!
//! The layout is saved in the browser; the URL shows the focused column.
//! It also tracks attention: sessions that finished or failed while not
//! shown, and pinned sessions, for the dock, the inbox and mission control;
//! and it tells the head which finished sessions you've seen (for Work).

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
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "view", rename_all = "snake_case")]
pub enum View {
    Sessions,
    Session {
        id: String,
        /// Where to go once it loads (from a search hit).
        #[serde(default)]
        target: Option<Target>,
    },
    /// A session's changes (its folder's git status and diff), beside it.
    SessionChanges {
        id: String,
    },
    /// A session's terminals, opened in its folder.
    SessionTerminal {
        id: String,
    },
    /// A session's folder.
    SessionFiles {
        id: String,
    },
    /// What a session published; `output`: the one shown (empty: the newest).
    SessionOutputs {
        id: String,
        #[serde(default)]
        output: String,
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
    /// Your Ask conversations (docs/ask.md).
    Chats,
    Analytics,
    Activity,
    Memex,
    Mail,
    Chat,
    /// The messages behind a mail/chat reference or a saved Brief request,
    /// independent of whether the request is still in the Brief.
    Conversation {
        target: String,
    },
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
    /// Projects (docs/projects.md); with an id, that project.
    Projects {
        #[serde(default)]
        id: String,
    },
    /// Docs and decks (docs/docs.md); with an id, that document.
    Docs {
        #[serde(default)]
        id: String,
        /// A block to reveal, without replacing the document's editor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
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
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    encode(s)
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
                    p.push_str(&format!("#{}", enc(a)));
                }
                p
            }
            View::SessionChanges { id } => format!("/sessions/{id}/changes"),
            View::SessionTerminal { id } => format!("/sessions/{id}/terminal"),
            View::SessionFiles { id } => format!("/sessions/{id}/files"),
            View::SessionOutputs { id, output } if output.is_empty() => format!("/sessions/{id}/outputs"),
            View::SessionOutputs { id, output } => format!("/sessions/{id}/outputs?output={}", enc(output)),
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
            View::Chats => "/ask".into(),
            View::Analytics => "/analytics".into(),
            View::Activity => "/activity".into(),
            View::Memex => "/memex".into(),
            View::Mail => "/mail".into(),
            View::Chat => "/chat".into(),
            View::Conversation { target } => format!("/conversations/{}", encode(target)),
            View::Brief => "/brief".into(),
            View::Calendar => "/calendar".into(),
            View::Contacts => "/contacts".into(),
            View::Photos => "/photos".into(),
            View::Decisions => "/decisions".into(),
            View::Work { id } if id.is_empty() => "/work".into(),
            View::Work { id } => format!("/work/{id}"),
            View::Projects { id } if id.is_empty() => "/projects".into(),
            View::Projects { id } => format!("/projects/{id}"),
            View::Docs { id, .. } if id.is_empty() => "/docs".into(),
            View::Docs { id, target: None } => format!("/docs/{id}"),
            View::Docs { id, target: Some(block) } => format!("/docs/{id}#{}", enc(block)),
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
                for key in ["q", "kind", "worker", "open", "output", "block", "signed_in", "signed_in_with_link"] {
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
        let (href, hash) = href.split_once('#').unwrap_or((href, ""));
        let (path, q) = href.split_once('?').unwrap_or((href, ""));
        let query: HashMap<String, String> =
            q.split('&').filter_map(|p| p.split_once('=')).map(|(k, v)| (k.to_owned(), decode(v))).collect();
        Self::parse(&crate::place::strip(path), &query, hash)
    }

    fn parse(path: &str, query: &HashMap<String, String>, hash: &str) -> Self {
        let hash = decode_fragment(hash.trim_start_matches('#'));
        let get = |k: &str| query.get(k).cloned().unwrap_or_default();
        let parts: Vec<String> = path.trim_matches('/').split('/').map(decode).collect();
        let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
        match parts.as_slice() {
            ["sessions", id] => {
                let target = (query.contains_key("q") || query.contains_key("open") || !hash.is_empty()).then(|| Target {
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
            ["sessions", id, "changes"] => View::SessionChanges { id: (*id).to_owned() },
            ["sessions", id, "terminal"] => View::SessionTerminal { id: (*id).to_owned() },
            ["sessions", id, "files"] => View::SessionFiles { id: (*id).to_owned() },
            ["sessions", id, "outputs"] => View::SessionOutputs { id: (*id).to_owned(), output: get("output") },
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
            ["ask"] | ["chats"] => View::Chats,
            ["analytics"] => View::Analytics,
            ["activity"] => View::Activity,
            ["memex"] => View::Memex,
            ["mail"] => View::Mail,
            ["chat"] => View::Chat,
            ["conversations", target] => View::Conversation { target: (*target).to_owned() },
            ["brief"] => View::Brief,
            ["calendar"] => View::Calendar,
            ["contacts"] => View::Contacts,
            ["photos"] => View::Photos,
            ["decisions"] | ["todos"] => View::Decisions,
            ["work"] => View::Work { id: String::new() },
            ["work", id] => View::Work { id: (*id).to_owned() },
            ["projects"] => View::Projects { id: String::new() },
            ["projects", id] => View::Projects { id: (*id).to_owned() },
            ["docs"] => View::Docs { id: String::new(), target: None },
            ["docs", id] => View::Docs {
                id: (*id).to_owned(),
                target: Some(if hash.is_empty() { get("block") } else { hash }).filter(|b| !b.is_empty()),
            },
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
            View::Session { id, .. } => live.session(id).map(|s| s.title).unwrap_or_else(|| "Session".into()),
            // Its name, and the session's.
            View::SessionChanges { id } | View::SessionTerminal { id } | View::SessionFiles { id } | View::SessionOutputs { id, .. } => {
                let name = self.session_pane().map_or("", |(p, _)| p.name());
                match live.session(id).map(|s| s.title) {
                    Some(t) => format!("{name} · {t}"),
                    None => name.to_owned(),
                }
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
            View::Chats => "Ask".into(),
            View::Analytics => "Analytics".into(),
            View::Activity => "Activity".into(),
            View::Memex => "Memex".into(),
            View::Mail => "Mail".into(),
            View::Chat => "Chat".into(),
            View::Conversation { .. } => "Messages".into(),
            View::Brief => "Brief".into(),
            View::Calendar => "Calendar".into(),
            View::Contacts => "Contacts".into(),
            View::Photos => "Photos".into(),
            View::Decisions => "Decisions".into(),
            View::Work { id } if id.is_empty() => "Work".into(),
            View::Work { .. } => "Work item".into(),
            View::Projects { id } if id.is_empty() => "Projects".into(),
            View::Projects { .. } => "Project".into(),
            View::Docs { id, .. } if id.is_empty() => "Docs".into(),
            View::Docs { id, .. } => crate::docs::title_of(id).unwrap_or_else(|| "Document".into()),
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
            && self.session_pane().is_none()
            && !matches!(self, View::Collections { name } if !name.is_empty())
            && !matches!(self, View::Work { id } if !id.is_empty())
            && !matches!(self, View::Docs { id, .. } if !id.is_empty())
            && !matches!(self, View::Projects { id } if !id.is_empty())
            && !matches!(self, View::App { record, .. } if !record.is_empty())
    }

    /// A session's pane (Changes, Terminal, Files, Outputs): which, and
    /// whose.
    pub fn session_pane(&self) -> Option<(crate::panes::Pane, &str)> {
        use crate::panes::Pane;
        match self {
            View::SessionChanges { id } => Some((Pane::Changes, id)),
            View::SessionTerminal { id } => Some((Pane::Terminal, id)),
            View::SessionFiles { id } => Some((Pane::Files, id)),
            View::SessionOutputs { id, .. } => Some((Pane::Outputs, id)),
            _ => None,
        }
    }

    fn session_id(&self) -> Option<&str> {
        match self {
            View::Session { id, .. } => Some(id),
            _ => self.session_pane().map(|(_, id)| id),
        }
    }

    /// Opening a destination reuses its live pane, not a history entry.
    /// Targets are navigation within it; searches and explorer/app routes
    /// deliberately keep distinct queries, sections and records separate.
    fn same_destination(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Session { id: a, .. }, Self::Session { id: b, .. })
            | (Self::Docs { id: a, .. }, Self::Docs { id: b, .. })
            | (Self::SessionOutputs { id: a, .. }, Self::SessionOutputs { id: b, .. }) => a == b,
            (Self::Settings { .. }, Self::Settings { .. }) => true,
            _ => self == other,
        }
    }

    /// An untargeted link focuses without resetting where you were reading.
    fn navigates_existing(&self) -> bool {
        !matches!(self, Self::Session { target: None, .. } | Self::Docs { target: None, .. })
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
    /// Minimized: hidden (keeping already-loaded state), keeping its column
    /// and its place in it for when it comes back.
    #[serde(default)]
    pub minimized: bool,
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
    /// Focus mode: the focused tile fills the workspace, and only it shows;
    /// the tiles keep their places for when it ends.
    #[serde(default)]
    pub full: bool,
}

impl Layout {
    fn new(view: View) -> Self {
        let mut l = Layout { tiles: Vec::new(), focus: 0, next: 1, full: false };
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
            minimized: false,
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
            // A column only minimized tiles hold is free.
            let mut count = [0usize; COLUMNS];
            for t in self.tiles.iter().filter(|t| !t.minimized) {
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

    /// Whether a drop changes the visible layout. The preview and the move
    /// use the same rules, including when tiles disappear during a drag.
    fn check_drop(&self, key: u64, to: Drop) -> Result<(), &'static str> {
        let shown = |key| self.tile(key).filter(|t| !t.minimized && t.stack.is_some());
        let from = shown(key).ok_or("The dragged tile is no longer shown")?;
        let target = shown(to.target()).ok_or("The destination tile is no longer shown")?;
        if key == target.key {
            return Err("Already here");
        }
        let column: Vec<u64> = self.tiles.iter().filter(|t| !t.minimized && t.stack == from.stack).map(|t| t.key).collect();
        match to {
            Drop::Above(_) | Drop::Below(_) if from.stack == target.stack => {
                let i = column.iter().position(|&k| k == key).unwrap();
                let j = column.iter().position(|&k| k == target.key).unwrap();
                if (matches!(to, Drop::Above(_)) && i + 1 == j) || (matches!(to, Drop::Below(_)) && j + 1 == i) {
                    return Err("Already here");
                }
            }
            Drop::LeftOf(_) | Drop::RightOf(_) => {
                let mut columns: Vec<usize> = self.tiles.iter().filter(|t| !t.minimized).filter_map(|t| t.stack).collect();
                columns.sort_unstable();
                columns.dedup();
                if column.len() > 1 && columns.len() >= COLUMNS {
                    return Err("Four columns are already open; drop above or below a tile");
                }
                if column.len() == 1 {
                    let i = columns.iter().position(|c| Some(*c) == from.stack).unwrap();
                    let j = columns.iter().position(|c| Some(*c) == target.stack).unwrap();
                    if (matches!(to, Drop::LeftOf(_)) && i + 1 == j) || (matches!(to, Drop::RightOf(_)) && j + 1 == i) {
                        return Err("Already here");
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Moves tile `key` to where it was dropped, keeping focus where it was.
    fn move_tile(&mut self, key: u64, to: Drop) {
        if self.check_drop(key, to).is_err() {
            return;
        }
        let focused = self.tiles.get(self.focus).map(|t| t.key);
        let from = self.tiles.iter().position(|t| t.key == key).unwrap();
        let target = to.target();
        let target_stack = self.tile(target).unwrap().stack.unwrap();
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
                let shown: HashSet<usize> = self.tiles.iter().filter(|t| !t.minimized).filter_map(|t| t.stack).collect();
                let columns: HashSet<usize> = self.tiles.iter().filter_map(|t| t.stack).collect();
                // Reuse a vacant saved column if adding this one would
                // exceed the limit, or take the source's hidden neighbours
                // along when moving its column. Otherwise place() would
                // cap five saved columns to four, merging visible ones.
                let spare = tile
                    .stack
                    .filter(|c| !shown.contains(c))
                    .or_else(|| if columns.len() >= COLUMNS { columns.difference(&shown).copied().min() } else { None });
                let stack = if matches!(to, Drop::LeftOf(_)) { 2 * target_stack } else { 2 * target_stack + 2 };
                // Spread the columns out (c to 2c + 1), to make room on
                // either side of each.
                for t in &mut self.tiles {
                    t.stack = t.stack.map(|c| if Some(c) == spare { stack } else { 2 * c + 1 });
                }
                tile.stack = Some(stack);
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

    /// The tiles in reading order: the columns left to right, each top to
    /// bottom. Minimized tiles keep their places in it (and so their
    /// numbers: ⌘1–9, the dock's).
    pub fn reading_order(&self) -> Vec<usize> {
        let mut order: Vec<usize> = (0..self.tiles.len()).collect();
        order.sort_by_key(|&i| self.tiles[i].stack.unwrap_or(0));
        order
    }

    /// Where each tile shown goes. Columns only minimized tiles hold take
    /// no room; in focus mode the focused tile takes all of it.
    pub fn rects(&self) -> HashMap<u64, Rect> {
        let shown: Vec<&Tile> = self.tiles.iter().filter(|t| !t.minimized).collect();
        let mut columns: Vec<usize> = shown.iter().map(|t| t.stack.unwrap_or(0)).collect();
        columns.sort_unstable();
        columns.dedup();
        let placed: Vec<(usize, u64)> =
            shown.iter().map(|t| (columns.iter().position(|&c| c == t.stack.unwrap_or(0)).unwrap_or(0), t.used)).collect();
        let mut rects: HashMap<u64, Rect> = shown.iter().map(|t| t.key).zip(arrange(&placed)).collect();
        if self.full
            && let Some(t) = self.tiles.get(self.focus)
        {
            rects.insert(t.key, Rect { left: 0.0, top: 0.0, width: 1.0, height: 1.0 });
        }
        rects
    }

    /// Hides tile `ti`, unless it is the last one shown; the focus goes to
    /// the tile shown that was used last, and focus mode ends with it.
    fn minimize(&mut self, ti: usize) {
        if ti >= self.tiles.len() || self.tiles.iter().filter(|t| !t.minimized).count() <= 1 {
            return;
        }
        self.tiles[ti].minimized = true;
        if self.focus == ti {
            self.full = false;
        }
        self.settle_focus();
    }

    /// Brings tile `ti` back (if minimized) and focuses it.
    fn show(&mut self, ti: usize) {
        if let Some(t) = self.tiles.get_mut(ti) {
            t.minimized = false;
            self.focus = ti;
            self.touch(ti);
        }
    }

    /// The focus on a tile shown: the one used last (bringing that one back
    /// if every tile is minimized).
    fn settle_focus(&mut self) {
        if self.tiles.get(self.focus).is_some_and(|t| !t.minimized) {
            return;
        }
        let last =
            |minimized: bool| (0..self.tiles.len()).filter(|&i| self.tiles[i].minimized == minimized).max_by_key(|&i| self.tiles[i].used);
        match last(false).or_else(|| last(true)) {
            Some(i) => {
                self.tiles[i].minimized = false;
                self.focus = i;
            }
            None => self.focus = 0,
        }
    }

    /// Saved document copies collapse to the focused copy, else the first.
    /// Only current doc panes are removed, not sessions or history entries.
    fn restore_documents(&mut self) {
        let focused = self.tiles[self.focus].key;
        let mut docs = HashMap::<String, (u64, u64)>::new();
        for ti in [self.focus].into_iter().chain(0..self.tiles.len()) {
            let t = &self.tiles[ti];
            for c in t.columns.iter().filter(|c| ti == self.focus && c.key == t.active).chain(t.columns.iter()) {
                if let View::Docs { id, .. } = &c.view
                    && !id.is_empty()
                {
                    docs.entry(id.clone()).or_insert((t.key, c.key));
                }
            }
        }
        let keep = |c: &Column, tile: u64, docs: &HashMap<String, (u64, u64)>| match &c.view {
            View::Docs { id, .. } if !id.is_empty() => docs.get(id).is_none_or(|&owner| owner == (tile, c.key)),
            _ => true,
        };
        for t in &mut self.tiles {
            let original = t.columns.clone();
            let cursor = t.columns[t.cursor].key;
            t.columns.retain(|c| keep(c, t.key, &docs));
            if t.columns.len() == original.len() {
                continue;
            }
            if t.columns.is_empty() && (!t.back.is_empty() || !t.forward.is_empty()) {
                // A duplicate-only tile still owns independent navigation.
                // Walk to its nearest useful page rather than deleting that
                // history or grafting it onto the surviving document tile.
                let useful = |cs: &Vec<Column>| cs.iter().any(|c| keep(c, t.key, &docs));
                let destination = t
                    .back
                    .iter()
                    .rev()
                    .position(useful)
                    .map(|n| (true, n + 1))
                    .or_else(|| t.forward.iter().rev().position(useful).map(|n| (false, n + 1)));
                t.columns = original;
                if let Some((back, steps)) = destination {
                    for _ in 0..steps {
                        let next = if back { t.back.pop() } else { t.forward.pop() }.unwrap();
                        let now = std::mem::replace(&mut t.columns, next);
                        if back { t.forward.push(now) } else { t.back.push(now) }
                    }
                    // A revived, previously history-only doc now has a live
                    // owner too: later revivals must not duplicate it.
                    t.columns.retain(|c| {
                        if let View::Docs { id, .. } = &c.view
                            && !id.is_empty()
                        {
                            docs.entry(id.clone()).or_insert((t.key, c.key));
                        }
                        keep(c, t.key, &docs)
                    });
                } else {
                    // All its history is duplicate docs. Keep a Home shell
                    // with both stacks and the outgoing page still reachable.
                    t.forward.push(std::mem::take(&mut t.columns));
                    self.next += 1;
                    t.columns.push(Column { key: self.next, view: View::home() });
                }
                t.settle();
            } else if !t.columns.is_empty() {
                t.cursor = t
                    .columns
                    .iter()
                    .position(|c| c.key == cursor)
                    .or_else(|| t.columns.iter().position(|c| c.key == t.active))
                    .unwrap_or(0);
                if !t.columns.iter().any(|c| c.key == t.active) {
                    t.active = t.columns[t.cursor].key;
                }
            }
        }
        self.tiles.retain(|t| !t.columns.is_empty());
        self.focus = self.tiles.iter().position(|t| t.key == focused).unwrap_or(0);
        self.place();
        self.settle_focus();
    }

    /// Closes tile `ti`; the focus stays where it was, or goes to the tile
    /// shown that was used last. The last tile closing leaves the home view.
    fn remove_tile(&mut self, ti: usize) {
        if self.tiles.len() == 1 {
            // A replacement, not the old tile with new contents: keep keys
            // monotonic so its ghost can exit while Home is already live.
            let home = self.new_tile(View::home());
            self.tiles = vec![home];
            self.focus = 0;
            self.full = false;
            self.place();
            return;
        }
        let focused = self.tiles.get(self.focus).map(|t| t.key).filter(|_| self.focus != ti);
        self.tiles.remove(ti);
        self.place();
        self.focus = focused.and_then(|k| self.tiles.iter().position(|t| t.key == k)).unwrap_or(usize::MAX);
        self.settle_focus();
    }

    /// The origin, then the focused tile, then other live panes. Opening
    /// from an explicit copy never jumps to another live copy.
    fn find_view(&self, origin: usize, view: &View) -> Option<(usize, usize)> {
        [origin, self.focus].into_iter().chain(0..self.tiles.len()).find_map(|ti| {
            let t = self.tiles.get(ti)?;
            let ci = [t.cursor]
                .into_iter()
                .chain(0..t.columns.len())
                .find(|&ci| t.columns.get(ci).is_some_and(|c| c.view.same_destination(view)))?;
            Some((ti, ci))
        })
    }

    fn reuse(&mut self, origin: usize, view: &View) -> bool {
        let Some((ti, ci)) = self.find_view(origin, view) else { return false };
        let t = &mut self.tiles[ti];
        if view.navigates_existing() && t.columns[ci].view != *view {
            let mut columns = t.columns.clone();
            columns[ci].view = view.clone();
            t.go(columns);
        }
        let (tile, col) = (t.key, t.columns[ci].key);
        self.reveal(tile, col);
        true
    }

    /// One open path for links, mentions, navigation and created documents.
    /// Explicit copies win over dedupe, even for sessions. Only a new target
    /// changes the reused pane's history; merely focusing it does not.
    fn open(&mut self, from: Option<(u64, u64)>, view: View, how: How) {
        let origin = from.and_then(|(t, _)| self.tiles.iter().position(|x| x.key == t)).unwrap_or(self.focus);
        if how != How::NewTile && self.reuse(origin, &view) {
            return;
        }
        if how == How::Instead {
            let key = self.key();
            self.tiles[origin].go(vec![Column { key, view }]);
            self.show(origin);
            return;
        }
        if how != How::Beside {
            self.open_tile(view);
            return;
        }
        let key = self.key();
        let t = &mut self.tiles[origin];
        let list = t.columns[0].clone();
        let next = match from {
            Some(_) => vec![list, Column { key, view }],
            None if !view.is_section() && list.view.is_section() => vec![list, Column { key, view }],
            None => vec![Column { key, view }],
        };
        if t.columns.iter().map(|c| &c.view).ne(next.iter().map(|c| &c.view)) {
            t.go(next);
        }
        t.settle();
        self.show(origin);
    }

    fn step(&mut self, delta: isize) {
        // A retained history entry must not silently recreate a document
        // copy. Reveal its live destination instead; the source's entire
        // snapshot and both stacks remain available, including other panes.
        let source = &self.tiles[self.focus];
        let next = if delta < 0 { source.back.last() } else { source.forward.last() };
        let existing = next.and_then(|cs| {
            cs.iter().find_map(|c| {
                if !matches!(&c.view, View::Docs { id, .. } if !id.is_empty())
                    || source.columns.iter().any(|now| now.view.same_destination(&c.view))
                {
                    return None;
                }
                self.tiles
                    .iter()
                    .enumerate()
                    .any(|(ti, t)| ti != self.focus && t.columns.iter().any(|now| now.view.same_destination(&c.view)))
                    .then(|| c.view.clone())
            })
        });
        if let Some(view) = existing {
            self.reuse(self.focus, &view);
            return;
        }
        let t = &mut self.tiles[self.focus];
        let active = t.active;
        let keys: Vec<_> = t.columns.iter().map(|c| c.key).collect();
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
        // Old snapshots can themselves contain two copies in one tile.
        // Normalize only the newly current panes, never stored snapshots.
        let mut docs = HashSet::new();
        t.columns.retain(|c| match &c.view {
            View::Docs { id, .. } if !id.is_empty() => docs.insert(id.clone()),
            _ => true,
        });
        if t.cursor >= t.columns.len() || !t.columns.iter().any(|c| c.key == t.active) {
            t.settle();
        }
        // A target-only visit retains the pane keys. History must keep
        // that pane active too, even when it is the left of two panes.
        if t.columns.iter().map(|c| c.key).eq(keys)
            && let Some(ci) = t.columns.iter().position(|c| c.key == active)
        {
            t.cursor = ci;
            t.active = active;
        }
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
        for t in self.tiles.iter().filter(|t| !t.minimized) {
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

    /// The pane a tile's bar speaks for: its last, or the session that a
    /// pane of its (Changes, Terminal…) is open beside.
    pub fn bar_column(&self, tile: u64) -> Option<u64> {
        let t = self.tile(tile)?;
        let last = t.columns.last()?;
        let session = last
            .view
            .session_pane()
            .and_then(|(_, id)| t.columns.iter().find(|c| matches!(&c.view, View::Session { id: x, .. } if x == id)).map(|c| c.key));
        Some(session.unwrap_or(last.key))
    }

    /// Where a session's pane is open, if it is: (tile key, pane key).
    pub fn find_pane(&self, pane: crate::panes::Pane, id: &str) -> Option<(u64, u64)> {
        self.tiles.iter().find_map(|t| t.columns.iter().find(|c| c.view.session_pane() == Some((pane, id))).map(|c| (t.key, c.key)))
    }

    /// Whether pane `col` of tile `tile` is on screen: its tile shown (not
    /// minimized, nor behind another in focus mode) and, where a tile shows
    /// one pane at a time (`one_pane`: a phone), the pane it shows.
    pub fn pane_shown(&self, tile: u64, col: u64, one_pane: bool) -> bool {
        let Some(ti) = self.tiles.iter().position(|t| t.key == tile) else { return false };
        let t = &self.tiles[ti];
        let Some(ci) = t.columns.iter().position(|c| c.key == col) else { return false };
        !t.minimized && (!self.full || self.focus == ti) && (!one_pane || t.cursor == ci)
    }

    /// Brings pane `col` of tile `tile` forward: its tile shown and focused,
    /// and the pane the one a phone shows.
    fn reveal(&mut self, tile: u64, col: u64) {
        let Some(ti) = self.tiles.iter().position(|t| t.key == tile) else { return };
        let Some(ci) = self.tiles[ti].columns.iter().position(|c| c.key == col) else { return };
        self.tiles[ti].cursor = ci;
        self.tiles[ti].active = col;
        self.show(ti);
    }

    /// Opens `view` beside pane `col` of tile `tile`, keeping that pane:
    /// the tile shows the two (a list that was beside it goes back in the
    /// tile's history).
    fn open_after(&mut self, tile: u64, col: u64, view: View) {
        let Some(ti) = self.tiles.iter().position(|t| t.key == tile) else { return };
        let Some(me) = self.tiles[ti].columns.iter().find(|c| c.key == col).cloned() else { return };
        // General detail links (Glance's worker, a started session, docs)
        // obey the same destination policy. Session companions deliberately
        // stay beside the selected transcript rather than stealing its list.
        if view.session_pane().is_none() && self.reuse(ti, &view) {
            return;
        }
        let key = self.key();
        let t = &mut self.tiles[ti];
        let next = vec![me, Column { key, view }];
        if t.columns.iter().map(|c| &c.view).ne(next.iter().map(|c| &c.view)) {
            t.go(next);
        }
        t.settle();
        self.focus = ti;
        self.touch(ti);
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
    /// In place of everything its tile shows (Back returns to it).
    Instead,
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

impl Drop {
    fn target(self) -> u64 {
        match self {
            Self::Above(k) | Self::Below(k) | Self::LeftOf(k) | Self::RightOf(k) => k,
        }
    }

    /// The outer fifths make a new column; the middle stacks tiles.
    fn at(key: u64, x: f64, y: f64) -> Self {
        match () {
            _ if x < 0.2 => Self::LeftOf(key),
            _ if x > 0.8 => Self::RightOf(key),
            _ if y < 0.5 => Self::Above(key),
            _ => Self::Below(key),
        }
    }
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
    /// Explicit target visits, including a second click on the same target.
    /// Transient: restoring a layout does not replay another pane's visit.
    navigation: RwSignal<(u64, u64)>,
    /// A request to open, consumed by the active Brief. Never part of its view.
    pub brief_request: RwSignal<Option<i64>>,
    /// What each session pane offers its tile's bar (its actions, ⋯), by
    /// pane key, while it is shown.
    pub chrome: RwSignal<HashMap<u64, crate::session::SessionChrome>>,
    /// Sessions that finished or failed while not shown, newest first.
    pub attention: RwSignal<Vec<String>>,
    pub pinned: RwSignal<Vec<String>>,
    /// The tile being dragged by its bar, and where it would drop.
    pub dragging: RwSignal<Option<u64>>,
    pub drop_at: RwSignal<Option<Drop>>,
    /// The catch-ups of sessions you came back to, by session.
    pub catchups: RwSignal<HashMap<String, crate::catchup::Card>>,
    /// When this page last had each session on screen.
    pub looked: StoredValue<crate::catchup::Looked>,
    /// Counts the views the head has taken (recently viewed loads again).
    pub viewed: RwSignal<u32>,
}

fn load<T: for<'de> Deserialize<'de>>(key: &str) -> Option<T> {
    serde_json::from_str(&storage_get(key)?).ok()
}

// Layout is committed by Leptos immediately. Only its picture catches up. Keep
// this specific to workspace roots: no transcript observation or per-frame
// layout reads, and never wait for an animation to mount/focus an input.
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(inline_js = r#"
let workspaceMotion;
const tileEaseMove = 'cubic-bezier(.3,0,0,1)';
function tileMotionMode() {
    const motion = window.c6Motion;
    if (motion?.off() || document.documentElement.dataset.motion === 'off') return 'off';
    return (motion ? motion.reduced() : matchMedia('(prefers-reduced-motion: reduce)').matches) ? 'reduced' : 'full';
}
function tileMotionNodes(root) {
    const nodes = [];
    for (const tile of root.children) {
        if (!tile.hasAttribute('data-tile-motion')) continue;
        nodes.push(tile);
        // TileView's section ends with the pane host. Walk just these roots,
        // not the potentially very large transcript/content trees inside them.
        const host = tile.firstElementChild?.lastElementChild;
        if (!host?.hasAttribute('data-pane-host')) continue;
        for (const pane of host.children) if (pane.hasAttribute('data-pane-motion')) nodes.push(pane);
    }
    return nodes;
}
function tileMotionKey(el) {
    return el.hasAttribute('data-tile-motion') ? 't' + el.dataset.tileMotion : 'p' + el.dataset.paneMotion;
}
function tileMotionCancel(state) {
    if (state.frame) cancelAnimationFrame(state.frame);
    state.frame = 0;
    state.before = null;
    for (const animation of state.animations.values()) animation.cancel();
    state.animations.clear();
}
export function tileMotionStop() {
    if (!workspaceMotion) return;
    tileMotionCancel(workspaceMotion);
    window.removeEventListener('c6-motion', workspaceMotion.preference);
    workspaceMotion = undefined;
}
function tileMotionAnimate(state, el, frames, duration, easing, done = () => {}) {
    if (!el.animate || !duration) { done(); return; }
    const animation = el.animate(frames, { duration, easing });
    state.animations.set(el, animation);
    animation.onfinish = animation.oncancel = () => {
        if (state.animations.get(el) === animation) state.animations.delete(el);
        done();
    };
}
function tileMotionReveal(state, tile, duration) {
    const surface = tile.firstElementChild;
    if (!surface) return;
    // Fade an empty veil OFF the final, opaque surface, never fade the tile
    // itself. Text reflows once at full size, with no outgoing text underneath.
    // Prepending preserves the final pane-host child used by tileMotionNodes.
    const veil = document.createElement('div');
    veil.setAttribute('data-tile-reveal', '');
    veil.setAttribute('aria-hidden', 'true');
    Object.assign(veil.style, {
        position: 'absolute', inset: '0', zIndex: '20', pointerEvents: 'none',
        borderRadius: 'inherit', background: getComputedStyle(surface).backgroundColor,
    });
    surface.prepend(veil);
    tileMotionAnimate(state, veil, [{ opacity: 1 }, { opacity: 0 }], duration, 'linear', () => veil.remove());
}
export function tileMotionPrepare() {
    const root = document.querySelector('[data-workspace-motion]');
    if (!root) return;
    if (workspaceMotion?.root !== root) {
        tileMotionStop();
        const state = { root, before: null, frame: 0, animations: new Map() };
        state.preference = () => { if (tileMotionMode() !== 'full') tileMotionCancel(state); };
        window.addEventListener('c6-motion', state.preference);
        workspaceMotion = state;
    }
    const state = workspaceMotion;
    const mode = tileMotionMode();
    if (mode === 'off') { tileMotionCancel(state); return; }
    if (!state.before) {
        // Read all displayed rects BEFORE cancelling interrupted animations.
        // Batch reads, then writes; one first/last measurement per action.
        state.before = new Map();
        for (const el of tileMotionNodes(root)) {
            const rect = el.getBoundingClientRect();
            if (!rect.width || !rect.height) continue;
            state.before.set(tileMotionKey(el), { el, rect });
        }
        for (const animation of state.animations.values()) animation.cancel();
    }
    // Removed/hidden roots disappear immediately. No cloned forms, text or
    // translucent exit pictures can cover the new layout (or join radio groups).
    if (state.frame) return;
    state.frame = requestAnimationFrame(() => {
        state.frame = 0;
        const before = state.before;
        state.before = null;
        if (!root.isConnected || tileMotionMode() === 'off') return;
        const reduced = tileMotionMode() === 'reduced';
        const after = new Map();
        for (const el of tileMotionNodes(root)) {
            const rect = el.getBoundingClientRect();
            if (rect.width && rect.height) after.set(tileMotionKey(el), { el, rect });
        }
        const reveals = new Set(), moves = [];
        for (const [key, item] of after) {
            const old = before.get(key);
            const tile = item.el.closest('[data-tile-motion]');
            const parentKey = tile !== item.el ? tileMotionKey(tile) : null;
            if (!old) {
                reveals.add(tile);
                continue;
            }
            const a = item.rect, b = old.rect;
            if (Math.abs(b.width - a.width) > .5 || Math.abs(b.height - a.height) > .5) {
                reveals.add(tile);
                continue;
            }
            let dx = b.left - a.left, dy = b.top - a.top;
            // Same-size moves translate only. The tile already moves its pane;
            // compensate that translation without ever scaling either root.
            const bp = before.get(parentKey)?.rect, ap = after.get(parentKey)?.rect;
            if (bp && ap) {
                dx -= bp.left - ap.left;
                dy -= bp.top - ap.top;
            }
            if (Math.abs(dx) < .5 && Math.abs(dy) < .5) continue;
            if (reduced) reveals.add(tile);
            else moves.push({ el: item.el, tile, dx, dy });
        }
        // Last reads are complete. Tile contents NEVER scale, including entry,
        // restore and neighbours growing back. A reveal keeps the background
        // opaque throughout; it cannot double-expose text from another tile.
        for (const tile of reveals) tileMotionReveal(state, tile, reduced ? 80 : 100);
        for (const { el, tile, dx, dy } of moves) if (!reveals.has(tile)) {
            tileMotionAnimate(state, el, [
                { transform: `translate(${dx}px,${dy}px)` }, { transform: 'none' },
            ], 220, tileEaseMove);
        }
    });
}
"#)]
extern "C" {
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = tileMotionPrepare)]
    fn tile_motion_prepare(layout: &str);
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = tileMotionStop)]
    fn tile_motion_stop();
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(PartialEq, Serialize)]
struct MotionTile {
    key: u64,
    rect: [f64; 4],
    desktop: bool,
    phone: bool,
    panes: Vec<(u64, bool)>,
    even: bool,
}

#[cfg(any(target_arch = "wasm32", test))]
fn motion_layout(layout: &Layout) -> Vec<MotionTile> {
    let rects = layout.rects();
    layout
        .tiles
        .iter()
        .enumerate()
        .map(|(i, tile)| {
            let rect = rects.get(&tile.key).copied().unwrap_or_default();
            MotionTile {
                key: tile.key,
                rect: [rect.left, rect.top, rect.width, rect.height],
                desktop: !tile.minimized && (!layout.full || layout.focus == i),
                phone: !tile.minimized && layout.focus == i,
                panes: tile.columns.iter().enumerate().map(|(i, col)| (col.key, tile.cursor == i)).collect(),
                even: tile.columns.len() == 2 && layout.bar_column(tile.key) == Some(tile.columns[0].key),
            }
        })
        .collect()
}

impl Workspace {
    /// Capture before notifying reactive layout consumers. The rAF callback
    /// reads their final geometry, without deferring layout, mounting or focus.
    fn update_layout(&self, change: impl FnOnce(&mut Layout)) {
        self.layout.update(|layout| {
            #[cfg(target_arch = "wasm32")]
            let before = motion_layout(layout);
            change(layout);
            #[cfg(target_arch = "wasm32")]
            {
                let after = motion_layout(layout);
                if before != after {
                    tile_motion_prepare(&serde_json::to_string(&after).unwrap_or_default());
                }
            }
        });
    }

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
                l.restore_documents();
                l
            }
        };
        let ws = Workspace {
            layout: RwSignal::new(layout),
            navigation: RwSignal::new((0, 0)),
            brief_request: RwSignal::new(request),
            chrome: RwSignal::new(HashMap::new()),
            attention: RwSignal::new(load(ATTENTION_KEY).unwrap_or_default()),
            pinned: RwSignal::new(load(PINNED_KEY).unwrap_or_default()),
            dragging: RwSignal::new(None),
            drop_at: RwSignal::new(None),
            viewed: RwSignal::new(0),
            catchups: RwSignal::new(HashMap::new()),
            looked: StoredValue::new(crate::catchup::Looked::restore()),
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
        // Hiding/removing the source can prevent its bar receiving dragend.
        Effect::new(move || {
            if let Some(key) = ws.dragging.get()
                && !ws.layout.with(|l| l.tile(key).is_some_and(|t| !t.minimized))
            {
                ws.dragging.set(None);
                ws.drop_at.set(None);
            }
        });
        ws
    }

    /// Opens `view` from pane `from` (tile key, pane key), or from outside
    /// the tiles (the header, a notification).
    ///
    /// One rule: ⌘/Ctrl-click (`NewTile`) makes a new tile; anything else
    /// stays in its tile. From a pane, it opens in the tile's right pane
    /// (beside a list, or in place of the item there). From outside, it
    /// replaces the focused tile (an item goes beside the list it shows).
    /// An already-open destination is focused in either pane; `NewTile`
    /// explicitly makes a second copy, including sessions and documents.
    pub fn open(&self, from: Option<(u64, u64)>, view: View, how: How) {
        let targeted = matches!(&view, View::Docs { target: Some(_), .. } | View::Session { target: Some(_), .. });
        self.update_layout(|l| l.open(from, view, how));
        self.visit_target(targeted);
    }

    fn visit_target(&self, targeted: bool) {
        if targeted && let Some(col) = self.layout.with_untracked(|l| l.tiles.get(l.focus).map(|t| t.active)) {
            self.navigation.update(|(visit, pane)| {
                *visit = visit.wrapping_add(1);
                *pane = col;
            });
        }
    }

    /// Only this pane's visits notify it. Retain the last counter when a
    /// different pane navigates, so its editor/scroll position stays put.
    pub(crate) fn navigation_for(&self, col: u64) -> Memo<u64> {
        let navigation = self.navigation;
        Memo::new(move |last: Option<&u64>| {
            let (visit, pane) = navigation.get();
            if pane == col { visit } else { last.copied().unwrap_or_default() }
        })
    }

    /// Opens `view` beside pane `col` of tile `tile`, keeping that pane (a
    /// session's Changes beside its transcript).
    pub fn open_after(&self, tile: u64, col: u64, view: View) {
        let targeted = matches!(&view, View::Docs { target: Some(_), .. } | View::Session { target: Some(_), .. });
        self.update_layout(|l| l.open_after(tile, col, view));
        self.visit_target(targeted);
    }

    /// Brings a pane forward (its tile shown and focused; on a phone, the
    /// pane shown).
    pub fn reveal(&self, tile: u64, col: u64) {
        self.update_layout(|l| l.reveal(tile, col));
    }

    /// Moves the item a tile shows (its right pane) into a tile of its own.
    pub fn detach(&self, tile: u64) {
        self.update_layout(|l| {
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
        self.update_layout(|l| l.step(delta));
        // A history collision can reveal an already-targeted document.
        // Revisit that block even if its address did not change.
        let targeted = self.layout.with_untracked(|l| matches!(l.focused_view(), Some(View::Docs { target: Some(_), .. })));
        self.visit_target(targeted);
    }

    /// On a phone, from the item back to the list beside it.
    pub fn show_list(&self, tile: u64) {
        self.update_layout(|l| {
            if let Some(t) = l.tile_mut(tile) {
                t.cursor = 0;
                t.active = t.columns[0].key;
            }
        });
    }

    /// Changes what a column shows in place (e.g. a search's query).
    pub fn replace(&self, tile: u64, col: u64, view: View) {
        self.update_layout(|l| {
            if let Some(c) = l.tile_mut(tile).and_then(|t| t.columns.iter_mut().find(|c| c.key == col)) {
                c.view = view;
            }
        });
    }

    /// Closes a pane: the item (the list stays), or the list (the item
    /// takes the tile); a tile's last pane closes the tile.
    pub fn close_column(&self, tile: u64, col: u64) {
        self.update_layout(|l| {
            let Some(ti) = l.tiles.iter().position(|t| t.key == tile) else { return };
            let t = &mut l.tiles[ti];
            let Some(i) = t.columns.iter().position(|c| c.key == col) else { return };
            if t.columns.len() > 1 {
                let mut rest = t.columns.clone();
                rest.remove(i);
                t.go(rest);
                return;
            }
            l.remove_tile(ti);
        });
    }

    pub fn close_tile(&self, tile: u64) {
        self.update_layout(|l| {
            let Some(ti) = l.tiles.iter().position(|t| t.key == tile) else { return };
            l.remove_tile(ti);
        });
    }

    /// Minimizes a tile (not the last one shown).
    pub fn minimize(&self, tile: u64) {
        self.update_layout(|l| {
            if let Some(ti) = l.tiles.iter().position(|t| t.key == tile) {
                l.minimize(ti);
            }
        });
    }

    /// Brings a tile back, if minimized, and focuses it.
    pub fn show_tile(&self, tile: u64) {
        self.update_layout(|l| {
            if let Some(ti) = l.tiles.iter().position(|t| t.key == tile) {
                l.show(ti);
            }
        });
    }

    /// The number of the tile a session is open in (1 for the first in
    /// reading order), if it is.
    pub fn tile_number(&self, id: &str) -> Option<usize> {
        self.layout
            .with(|l| {
                l.reading_order()
                    .iter()
                    .position(|&ti| l.tiles[ti].columns.iter().any(|c| matches!(&c.view, View::Session { id: x, .. } if x == id)))
            })
            .map(|n| n + 1)
    }

    /// Minimized tiles in reading order: (key, number, what it shows).
    pub fn minimized(&self) -> Vec<(u64, usize, Vec<View>)> {
        self.layout.with(|l| {
            l.reading_order()
                .into_iter()
                .enumerate()
                .filter(|(_, ti)| l.tiles[*ti].minimized)
                .map(|(n, ti)| (l.tiles[ti].key, n + 1, l.tiles[ti].columns.iter().map(|c| c.view.clone()).collect()))
                .collect()
        })
    }

    /// Moves a dragged tile to where it was dropped.
    pub fn move_tile(&self, key: u64, to: Drop) {
        self.update_layout(|l| l.move_tile(key, to));
    }

    pub fn new_tile(&self) {
        self.open(None, View::home(), How::NewTile);
    }

    /// Focus mode on (for the tile `key`) or off: in it, the focused tile
    /// fills the workspace (and stays in it as the focus moves to another).
    pub fn set_full(&self, key: Option<u64>) {
        self.update_layout(|l| {
            if let Some(i) = key.and_then(|k| l.tiles.iter().position(|t| t.key == k)) {
                l.show(i);
            }
            l.full = key.is_some();
        });
    }

    pub fn is_full(&self) -> bool {
        self.layout.with(|l| l.full)
    }

    /// Focuses the `n`th tile in reading order (from 0), bringing it back
    /// if minimized.
    pub fn focus_tile(&self, n: usize) {
        let Some(ti) = self.layout.with_untracked(|l| l.reading_order().get(n).copied()) else { return };
        if self.layout.with_untracked(|l| l.focus != ti || l.tiles[ti].minimized) {
            self.update_layout(|l| l.show(ti));
        }
    }

    /// Focuses a tile you pressed on, without growing it yet: that waits
    /// for the click to finish ([`Workspace::grow`]), so what you clicked
    /// doesn't move away under the pointer.
    fn focus_tile_key(&self, key: u64) {
        if let Some(i) = self.layout.with_untracked(|l| l.tiles.iter().position(|t| t.key == key))
            && self.layout.with_untracked(|l| l.focus != i)
        {
            self.update_layout(|l| l.focus = i);
        }
    }

    /// Marks a tile the last used (in a tall column, it takes half the
    /// height).
    fn grow(&self, key: u64) {
        let latest = self.layout.with_untracked(|l| l.tile(key).is_none_or(|t| t.used == l.next));
        if !latest && let Some(i) = self.layout.with_untracked(|l| l.tiles.iter().position(|t| t.key == key)) {
            self.update_layout(|l| l.touch(i));
        }
    }

    pub fn set_active(&self, tile: u64, col: u64) {
        if self.layout.with_untracked(|l| l.tile(tile).is_some_and(|t| t.active != col)) {
            self.update_layout(|l| {
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

    /// When this page last had a session on screen.
    pub fn looked_at(&self, id: &str) -> Option<i64> {
        self.looked.with_value(|l| l.get(id))
    }

    /// These sessions are on screen now, or were until now (`save`: kept
    /// in the browser at once).
    pub fn mark_looked<'a>(&self, ids: impl IntoIterator<Item = &'a String>, save: bool) {
        let now = crate::util::now_ms();
        self.looked.update_value(|l| l.note(ids, now, save));
    }

    pub fn dismiss(&self, id: &str) {
        self.attention.update(|a| a.retain(|x| x != id));
    }

    /// Flags sessions that finish or fail while not shown, and clears the
    /// flag once they are.
    pub fn watch(&self, live: LiveFeed) {
        let ws = *self;
        // Header resolution follows the same initial-load budget as panes.
        // It still resolves omitted/archived rows after the focused paint.
        let loading = TileLoading {
            released: RwSignal::new(false),
            drawn: RwSignal::new(HashSet::new()),
            one_pane: RwSignal::new(crate::util::phone()),
        };
        provide_context(loading);
        // Open views own their metadata, not their membership in a recent,
        // non-archived list. This also covers restored layouts, direct links,
        // minimized tiles and standalone session panes. Keep the last good
        // row while resolving an omission; never close a view because of it.
        let open = Memo::new(move |_| {
            ws.layout.with(|l| {
                l.tiles.iter().flat_map(|t| &t.columns).filter_map(|c| c.view.session_id().map(str::to_owned)).collect::<HashSet<_>>()
            })
        });
        let fetching = StoredValue::new(HashSet::<String>::new());
        let retry = RwSignal::new(0u64);
        Effect::new(move || {
            retry.track();
            let ids = open.get();
            let eligible = ws.layout.with(|l| initial_metadata_ids(l, loading.one_pane.get(), loading.released.get()));
            let sessions = live.sessions.get();
            let mut cached = live.resolved_sessions.get_untracked();
            cached.retain(|id, _| ids.contains(id));
            for s in sessions.iter().filter(|s| ids.contains(&s.id)) {
                cached.insert(s.id.clone(), s.clone());
            }
            if live.resolved_sessions.with_untracked(|old| *old != cached) {
                live.resolved_sessions.set(cached);
            }
            for id in ids.into_iter().filter(|id| eligible.contains(id) && !sessions.iter().any(|s| s.id == *id)) {
                // A slow by-id request must get to finish even if newer
                // live snapshots arrive; never pile up or starve requests.
                if fetching.try_update_value(|pending| pending.insert(id.clone())) != Some(true) {
                    continue;
                }
                // Capture the owned future while its arena handle is live.
                // A completed cached value must not suppress a fresh check.
                let pending = crate::resources::pending_session(&id).map(|data| std::future::IntoFuture::into_future(data.detail));
                leptos::task::spawn_local(async move {
                    let result = match pending {
                        Some(pending) => pending.await,
                        None => api::session(id.clone()).await,
                    };
                    let failed = result.is_err();
                    fetching.try_update_value(|pending| pending.remove(&id));
                    if let Ok(detail) = result
                        && open.try_with_untracked(|ids| ids.contains(&id)) == Some(true)
                        && live.sessions.try_with_untracked(|ss| !ss.iter().any(|s| s.id == id)) == Some(true)
                    {
                        live.remember_session(detail.session);
                    } else if failed {
                        // Live snapshots are change-driven. A transient
                        // read failure must recover without another change.
                        set_timeout(
                            move || {
                                if open.try_with_untracked(|ids| ids.contains(&id)) == Some(true) {
                                    let _ = retry.try_update(|n| *n = n.wrapping_add(1));
                                }
                            },
                            std::time::Duration::from_secs(2),
                        );
                    }
                });
            }
        });
        let previous = StoredValue::new(HashMap::<String, SessionStatus>::new());
        Effect::new(move || {
            let sessions = live.sessions.get();
            let visible = ws.layout.with_untracked(Layout::visible_sessions);
            let mut flagged = Vec::new();
            previous.update_value(|prev| {
                for s in &sessions {
                    let status = s.display_status();
                    let was = prev.insert(s.id.clone(), status);
                    let ended = was == Some(SessionStatus::Running) && matches!(status, SessionStatus::Idle | SessionStatus::Failed);
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
        // A session shown while its turn has ended (or it failed), with the
        // page in view: you've seen it, and the head stops counting it for
        // its work (docs/work.md §3). Told again when it changes.
        let page = RwSignal::new(!document().hidden());
        // It lives as long as the page does.
        let _ = window_event_listener_untyped("visibilitychange", move |_| page.set(!document().hidden()));
        let told = StoredValue::new(HashMap::<String, i64>::new());
        // The sessions in view (with the page): one that comes into view
        // again is one you came back to, and is asked first what it did
        // since it was last on screen here (its catch-up), then told seen.
        let in_view = StoredValue::new(HashSet::<String>::new());
        Effect::new(move || {
            let sessions = live.sessions.get();
            let visible = ws.layout.with(Layout::visible_sessions);
            if !page.get() {
                // Out of sight: on screen until now.
                in_view.with_value(|was| ws.mark_looked(was, true));
                in_view.set_value(HashSet::new());
                return;
            }
            let now: HashSet<String> = sessions.iter().filter(|s| visible.contains(&s.id)).map(|s| s.id.clone()).collect();
            let (back, gone): (HashSet<String>, Vec<String>) =
                in_view.with_value(|was| (now.difference(was).cloned().collect(), was.difference(&now).cloned().collect()));
            // When each one back was last on screen, read before it's on
            // screen again; those still in view, and those gone from it,
            // were on screen until now.
            let looked: HashMap<String, i64> = back.iter().filter_map(|id| Some((id.clone(), ws.looked_at(id)?))).collect();
            let looked_at = move |id: &str| looked.get(id).copied();
            ws.mark_looked(now.difference(&back), false);
            ws.mark_looked(&gone, true);
            in_view.set_value(now);
            for s in sessions.iter().filter(|s| back.contains(&s.id) && !matches!(s.status, SessionStatus::Idle | SessionStatus::Failed)) {
                leptos::task::spawn_local(crate::catchup::look(ws.catchups, s.id.clone(), looked_at(&s.id)));
            }
            for s in sessions.iter().filter(|s| visible.contains(&s.id) && matches!(s.status, SessionStatus::Idle | SessionStatus::Failed))
            {
                if told.with_value(|t| t.get(&s.id) == Some(&s.updated_at)) && !back.contains(&s.id) {
                    continue;
                }
                told.update_value(|t| {
                    t.insert(s.id.clone(), s.updated_at);
                });
                let id = s.id.clone();
                if back.contains(&id) {
                    let at = looked_at(&id);
                    leptos::task::spawn_local(async move {
                        crate::catchup::look(ws.catchups, id.clone(), at).await;
                        api::session_seen(id).await;
                    });
                } else {
                    leptos::task::spawn_local(api::session_seen(id));
                }
            }
        });
        // The focused session, with the page in view: you viewed it (the
        // head keeps the list, for every device).
        let focused = Memo::new(move |_| {
            ws.layout.with(|l| match l.focused_view() {
                Some(View::Session { id, .. }) => Some(id.clone()),
                _ => None,
            })
        });
        Effect::new(move || {
            if let (Some(id), true) = (focused.get(), page.get()) {
                leptos::task::spawn_local(async move {
                    if api::session_viewed(id).await {
                        let _ = ws.viewed.try_update(|n| *n += 1);
                    }
                });
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
        for s in sessions.iter().filter(|s| s.display_status() == SessionStatus::Running) {
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

    /// Whether its tile's bar speaks for it: it's the tile's last column,
    /// or one of its own panes is beside it (tracked: a column opened or
    /// closed beside it changes it).
    pub fn in_bar(&self) -> bool {
        let (tile, col) = (self.tile, self.col);
        self.ws.layout.with(|l| l.bar_column(tile) == Some(col))
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ScrollIntent {
    Away,
    Toward,
    Scrollbar,
}

impl ScrollIntent {
    fn from_delta(delta: f64) -> Option<Self> {
        if delta < 0.0 {
            Some(Self::Away)
        } else if delta > 0.0 {
            Some(Self::Toward)
        } else {
            None
        }
    }

    fn from_key(key: &str, shift: bool) -> Option<Self> {
        match key {
            "ArrowUp" | "PageUp" | "Home" => Some(Self::Away),
            "ArrowDown" | "PageDown" | "End" => Some(Self::Toward),
            " " => Some(if shift { Self::Away } else { Self::Toward }),
            _ => None,
        }
    }
}

/// Input decides whether to stop; only a user return to the very end resumes.
/// Upward input stops before the browser's default scroll, without needing a gap.
fn scroll_follow(following: bool, intent: Option<ScrollIntent>, gap: Option<i32>) -> bool {
    match (intent, gap) {
        (Some(ScrollIntent::Away), None) => false,
        (Some(ScrollIntent::Toward), Some(gap)) if gap < 4 => true,
        (Some(ScrollIntent::Scrollbar), Some(gap)) => gap < 4,
        _ => following,
    }
}

/// A scrolling area that remembers its position while its column is folded
/// away: where it was, or the end if it was at the end.
///
/// With `follow`, it keeps to the end as its content grows, while shown
/// again after being hidden, and as content renders late, until you
/// scroll away from the end yourself (wheel, touch, the scroll bar or
/// keys); back at the very end, it follows again.
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
                    if let Some(el) = node.try_get_untracked().flatten() {
                        if follow.map_or(end, |f| f.try_get_untracked() == Some(true)) {
                            stop_glide_to_end(&el);
                            el.set_scroll_top(el.scroll_height());
                        } else if top > 0 {
                            stop_glide_to_end(&el);
                            el.set_scroll_top(top);
                        }
                    }
                });
            }
        });
    }
    // Direction comes from input, never from scrollTop changes: those can be
    // caused by rendering, restoring a hidden tile, or our own smooth glide.
    let intent = StoredValue::new(None::<ScrollIntent>);
    let touch = StoredValue::new(None::<(i32, i32)>);
    if let Some(follow) = follow {
        Effect::new(move || {
            let following = follow.get();
            if !following && let Some(el) = node.get_untracked() {
                stop_glide_to_end(&el);
            }
            if following || intent.get_value() == Some(ScrollIntent::Toward) {
                // Opening / Latest explicitly follows; an anchor jump explicitly
                // stops. Neither should inherit a pending user return to the end.
                intent.set_value(None);
            }
        });
        type ResizeWatch = (web_sys::ResizeObserver, wasm_bindgen::closure::Closure<dyn Fn(js_sys::Array)>);
        let observer = StoredValue::new_local(None::<ResizeWatch>);
        let frame = StoredValue::new(None::<leptos::leptos_dom::helpers::AnimationFrameRequestHandle>);
        let stop = move || {
            observer.try_update_value(|o| {
                if let Some(watch) = o.take() {
                    watch.0.disconnect();
                }
            });
            if let Some(frame) = frame.try_update_value(Option::take).flatten() {
                frame.cancel();
            }
        };
        Effect::new(move || {
            let Some(el) = node.get() else { return };
            // One at a time: a box drawn anew isn't watched twice.
            stop();
            let to_end = wasm_bindgen::closure::Closure::<dyn Fn(js_sys::Array)>::new(move |_: js_sys::Array| {
                // Scrolling can materialize contained rows and resize again. Do
                // not measure or scroll inside resize delivery; coalesce a frame.
                let Some(None) = frame.try_get_value() else { return };
                let next = leptos::leptos_dom::helpers::request_animation_frame_with_handle(move || {
                    if frame.try_update_value(|f| *f = None).is_none() {
                        return;
                    }
                    // Input or unmount may have stopped following since delivery.
                    if let Some(el) = node.try_get_untracked().flatten()
                        && follow.try_get_untracked() == Some(true)
                        && el.client_height() > 0
                    {
                        glide_to_end(&el);
                    }
                })
                .ok();
                frame.set_value(next);
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
                observer.set_value(Some((obs, to_end)));
            }
        });
        on_cleanup(stop);
    }
    let yours = move |direction: ScrollIntent| {
        let (Some(f), Some(el)) = (follow, node.get_untracked()) else { return };
        if direction != ScrollIntent::Toward && el.scroll_height() <= el.client_height() {
            return;
        }
        intent.set_value(Some(direction));
        let now = f.get_untracked();
        // A downward input while already at the end need not emit a scroll
        // event (e.g. the prior upward trackpad movement rounded to zero px).
        let gap = (direction == ScrollIntent::Toward).then(|| el.scroll_height() - el.scroll_top() - el.client_height());
        let next = scroll_follow(now, Some(direction), gap);
        if now != next {
            f.set(next);
        }
        if matches!(direction, ScrollIntent::Away | ScrollIntent::Scrollbar) {
            // Cancel an already-running smooth glide without moving the box or
            // preventing the browser's wheel, touch or keyboard default action.
            let opts = web_sys::ScrollToOptions::new();
            opts.set_top(el.scroll_top() as f64);
            opts.set_behavior(web_sys::ScrollBehavior::Instant);
            el.scroll_to_with_scroll_to_options(&opts);
        }
    };
    view! {
        <div node_ref=node class=format!("min-h-0 flex-1 overflow-y-auto {class}")
            tabindex=follow.map(|_| 0)
            on:wheel=move |ev: web_sys::WheelEvent| {
                if let Some(direction) = ScrollIntent::from_delta(ev.delta_y()) {
                    yours(direction);
                }
            }
            on:touchstart=move |ev: web_sys::TouchEvent| {
                touch.set_value(ev.touches().item(0).map(|t| (t.identifier(), t.client_y())));
            }
            on:touchmove=move |ev: web_sys::TouchEvent| {
                let Some((id, y)) = touch.get_value() else { return };
                let touches = ev.touches();
                if let Some(t) = (0..touches.length()).filter_map(|i| touches.item(i)).find(|t| t.identifier() == id) {
                    touch.set_value(Some((id, t.client_y())));
                    // Finger down means the transcript moves up.
                    if let Some(direction) = ScrollIntent::from_delta((y - t.client_y()) as f64) {
                        yours(direction);
                    }
                }
            }
            on:touchend=move |_| touch.set_value(None)
            on:touchcancel=move |_| touch.set_value(None)
            on:keydown=move |ev: web_sys::KeyboardEvent| {
                let in_field = ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                    .is_some_and(|el| el.closest("input, textarea, select, [contenteditable]:not([contenteditable=false])").ok().flatten().is_some());
                if !ev.default_prevented() && !in_field
                    && let Some(direction) = ScrollIntent::from_key(&ev.key(), ev.shift_key())
                {
                    yours(direction);
                }
            }
            // The scroll bar: a press on the box itself, not its content.
            on:pointerdown=move |ev: web_sys::PointerEvent| {
                if ev.pointer_type() == "mouse"
                    && ev.target().is_some_and(|t| node.get_untracked().is_some_and(|el| AsRef::<web_sys::EventTarget>::as_ref(&el) == &t))
                {
                    yours(ScrollIntent::Scrollbar);
                }
            }
            on:scrollend=move |_| {
                // Keep permission through touch/trackpad inertia, not through a
                // later unrelated resize or programmatic scroll to the bottom.
                intent.set_value(None);
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
                let now = f.get_untracked();
                let gap = el.scroll_height() - el.scroll_top() - el.client_height();
                let next = scroll_follow(now, intent.get_value(), Some(gap));
                if now != next {
                    f.set(next);
                }
            }
            if let Some(f) = on_scroll {
                f.run(());
            }
        }>{children()}</div>
    }
}

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = c6GlideToEnd)]
    fn glide_to_end_js(el: &web_sys::Element);
    #[wasm_bindgen::prelude::wasm_bindgen(js_name = c6StopGlide)]
    fn stop_glide_to_end_js(el: &web_sys::Element);
}

/// Follows the end of a scroll box: a short glide down to what just arrived
/// (rather than a jump for each line), straight there when it is more than
/// a screen away (as when it opens) or motion is reduced. Shares the arrival's
/// 200ms out curve; JS measures only on invocation and cancels on user input.
pub fn glide_to_end(el: &web_sys::Element) {
    #[cfg(target_arch = "wasm32")]
    glide_to_end_js(el);
    #[cfg(not(target_arch = "wasm32"))]
    let _ = el;
}

/// Cancel a glide before a programmatic snap or anchor jump. Does not read
/// geometry or move the box; the caller can set its intended position now.
pub fn stop_glide_to_end(el: &web_sys::Element) {
    #[cfg(target_arch = "wasm32")]
    stop_glide_to_end_js(el);
    #[cfg(not(target_arch = "wasm32"))]
    let _ = el;
}

/// Whether a scrolling area is at its end (allowing a few pixels of rounding).
pub fn at_end(el: &web_sys::Element) -> bool {
    el.scroll_height() - el.scroll_top() - el.client_height() < 4
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
    let loading = expect_context::<TileLoading>();
    let facets = crate::aspects::use_facets();
    let resize = window_event_listener(leptos::ev::resize, move |_| {
        loading.one_pane.set(crate::util::phone());
    });
    on_cleanup(move || resize.remove());
    let drawn_focus = Memo::new(move |_| {
        if loading.released.get() {
            return None;
        }
        ws.layout.with(|l| loading.drawn.with(|drawn| focused_drawn(l, loading.one_pane.get(), drawn)))
    });
    Effect::new(move || {
        let Some(cols) = drawn_focus.get() else { return };
        // One frame lets the focused DOM and scroll restoration settle;
        // the next yields a paint before other tiles start their requests.
        request_animation_frame(move || {
            request_animation_frame(move || {
                if drawn_focus.try_get_untracked() == Some(Some(cols)) {
                    loading.released.try_set(true);
                }
            });
        });
    });
    let keys = window_event_listener(leptos::ev::keydown, move |ev| {
        if !crate::util::app_shortcut(&ev) {
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
    on_cleanup(move || keys.remove());
    #[cfg(target_arch = "wasm32")]
    on_cleanup(tile_motion_stop);
    let keys = Memo::new(move |_| ws.layout.with(|l| l.tiles.iter().map(|t| t.key).collect::<Vec<_>>()));
    // In focus mode the focused tile takes the whole workspace (the others
    // keep their places, hidden).
    let rects = Memo::new(move |_| ws.layout.with(Layout::rects));
    // Tiles are cards with room between them, placed by `arrange` (so one
    // moving to another column keeps what it shows); on a phone, one fills
    // the screen.
    view! {
        <div class="flex min-h-0 min-w-0 flex-1 bg-zinc-100 dark:bg-black md:p-1">
            <div data-workspace-motion class="relative flex min-h-0 min-w-0 flex-1">
                <For each=move || keys.get() key=|k| *k let:key>
                    <TileView key rect=Signal::derive(move || rects.with(|r| r.get(&key).copied().unwrap_or_default())) />
                </For>
                <Show when=move || crate::aspects::tile_bars_shown(facets.tile_bars.get(), loading.one_pane.get())>
                    <crate::aspects::TileMarks keys />
                </Show>
            </div>
        </div>
    }
}

/// Only initial loading is staged. Once mounted, a pane keeps its DOM,
/// subscriptions, drafts and scroll state even while hidden.
#[derive(Clone, Copy)]
struct TileLoading {
    released: RwSignal<bool>,
    drawn: RwSignal<HashSet<u64>>,
    one_pane: RwSignal<bool>,
}

fn initial_mount(mounted: bool, shown: bool, focused: bool, released: bool) -> bool {
    mounted || (shown && (focused || released))
}

fn initial_metadata_ids(layout: &Layout, one_pane: bool, released: bool) -> HashSet<String> {
    layout
        .tiles
        .iter()
        .enumerate()
        .filter(|(i, _)| released || *i == layout.focus)
        .flat_map(|(_, tile)| {
            tile.columns
                .iter()
                .filter(move |col| released || layout.pane_shown(tile.key, col.key, one_pane))
                .filter_map(|col| col.view.session_id().map(str::to_owned))
        })
        .collect()
}

fn focused_drawn(layout: &Layout, one_pane: bool, drawn: &HashSet<u64>) -> Option<Vec<u64>> {
    let tile = layout.tiles.get(layout.focus)?;
    let cols: Vec<_> = tile.columns.iter().filter(|c| layout.pane_shown(tile.key, c.key, one_pane)).map(|c| c.key).collect();
    (!cols.is_empty() && cols.iter().all(|c| drawn.contains(c))).then_some(cols)
}

/// A session supplies readiness after its detail and initial log replay (or
/// a disconnect notice), not its empty, still-loading shell.
#[derive(Clone, Copy)]
pub(crate) struct InitialDraw(pub RwSignal<bool>);

#[component]
fn InitialView(view: View, col: u64) -> impl IntoView {
    let loading = expect_context::<TileLoading>();
    let ready = RwSignal::new(!matches!(view, View::Session { .. }));
    provide_context(InitialDraw(ready));
    Effect::new(move || {
        if ready.get() && !loading.released.get_untracked() {
            loading.drawn.update(|drawn| {
                drawn.insert(col);
            });
        }
    });
    on_cleanup(move || {
        loading.drawn.try_update(|drawn| {
            drawn.remove(&col);
        });
    });
    render_view(view)
}

/// Where a popover opens: below the element clicked, kept on screen.
pub fn below(ev: &web_sys::MouseEvent, width: f64) -> (f64, f64) {
    let el = ev.current_target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok());
    below_element(el.as_ref(), width)
}

/// Where a popover opens: below an element, kept on screen.
pub fn below_element(el: Option<&web_sys::Element>, width: f64) -> (f64, f64) {
    let rect = el.map(|el| el.get_bounding_client_rect());
    let vw = window().inner_width().ok().and_then(|v| v.as_f64()).unwrap_or(1200.0);
    match rect {
        Some(r) => ((r.left()).min(vw - width - 8.0).max(8.0), r.bottom() + 4.0),
        None => (8.0, 48.0),
    }
}

/// A popover at a place on screen (from [`below`]); a click outside it
/// closes it, and so does Esc wherever focus is (focus going back to what
/// opened it).
#[component]
pub fn Popover(at: RwSignal<Option<(f64, f64)>>, #[prop(into)] class: String, children: ChildrenFn) -> impl IntoView {
    let class = StoredValue::new(class);
    let children = StoredValue::new(children);
    let pop = StoredValue::new(crate::util::escapable(
        move || at.with(Option::is_some),
        move || {
            at.try_set(None);
        },
    ));
    move || {
        at.get().map(|(x, y)| {
            view! {
                <leptos::portal::Portal>
                <div class="fixed inset-0 z-40" on:click=move |ev| { ev.stop_propagation(); at.set(None) }></div>
                <div data-pop=pop.get_value()
                    class=format!("c6-pop phone-popover fixed z-50 max-h-[75vh] overflow-y-auto rounded-lg border border-zinc-200 bg-white p-2 text-sm shadow-xl dark:border-zinc-700 dark:bg-zinc-900 {}", class.get_value())
                    style=format!("left:{x}px;top:{y}px;--c6-pop-y:{y}px;transform-origin:top left")
                >
                    {children.with_value(|c| c())}
                </div>
                </leptos::portal::Portal>
            }
        })
    }
}

#[component]
fn TileView(key: u64, rect: Signal<Rect>) -> impl IntoView {
    let ws = use_workspace();
    let bar_colour = expect_context::<crate::settings::SessionBarDisplay>().colour;
    let live = api::live();
    let focused = Memo::new(move |_| ws.layout.with(|l| l.tiles.get(l.focus).map(|t| t.key) == Some(key)));
    // Focus mode: this tile alone, or hidden behind the one that is.
    let full = Memo::new(move |_| ws.layout.with(|l| l.full));
    let many = Memo::new(move |_| ws.layout.with(|l| l.tiles.iter().filter(|t| !t.minimized).count() > 1 && !l.full));
    let minimized = Memo::new(move |_| ws.layout.with(|l| l.tile(key).is_some_and(|t| t.minimized)));
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
    // The session it shows (alone, as the item beside a list, or with one
    // of its panes beside it): its pane, and how it is now. The bar wears
    // its colour and says its state.
    let shown = Memo::new(move |_| {
        ws.layout.with(|l| {
            let col = l.bar_column(key)?;
            l.tile(key)?.columns.iter().find(|c| c.key == col).and_then(|c| match &c.view {
                View::Session { id, .. } => Some((col, id.clone())),
                _ => None,
            })
        })
    });
    let session = Memo::new(move |_| shown.get().and_then(|(_, id)| live.session(&id)));
    let colour = Memo::new(move |_| session.get().map(|s| crate::marks::of(&s)));
    let palette_at = RwSignal::new(None::<(f64, f64)>);
    let menu_at = RwSignal::new(None::<(f64, f64)>);
    let rename_at = RwSignal::new(None::<(f64, f64)>);
    let renamable = move || session.get().is_some_and(|s| s.parent_id.is_none() && !s.observed);
    let chrome = move || shown.get().and_then(|(col, _)| ws.chrome.with(|m| m.get(&col).copied()));
    // The rename box opens under the name (from ⋯ as from a double-click).
    let title_el = NodeRef::<leptos::html::Span>::new();
    let rename_under_title = move || {
        let el: Option<web_sys::Element> = title_el.get_untracked().map(Into::into);
        rename_at.set(Some(below_element(el.as_ref(), 320.0)));
    };
    let btn = crate::session::SESSION_SLOT;
    // Another tile dragged over this one: where it would go, by where the
    // pointer is (the outer fifths make a new column beside this one's).
    // Include the padding between tiles: it belongs to this drop target too.
    let drop_area = NodeRef::<Div>::new();
    let zone = move |ev: &web_sys::DragEvent| -> Option<Drop> {
        let r = drop_area.get_untracked()?.get_bounding_client_rect();
        let x = (f64::from(ev.client_x()) - r.left()) / r.width().max(1.0);
        let y = (f64::from(ev.client_y()) - r.top()) / r.height().max(1.0);
        Some(Drop::at(key, x, y))
    };
    let feedback = Memo::new(move |_| {
        let from = ws.dragging.get()?;
        let to = ws.drop_at.get().filter(|to| to.target() == key)?;
        Some(ws.layout.with(|l| l.check_drop(from, to)))
    });
    let hint = move || match ws.drop_at.get().filter(|_| feedback.get() == Some(Ok(()))) {
        Some(Drop::Above(k)) if k == key => Some("inset-x-0 top-0 h-1/2"),
        Some(Drop::Below(k)) if k == key => Some("inset-x-0 bottom-0 h-1/2"),
        Some(Drop::LeftOf(k)) if k == key => Some("inset-y-0 left-0 w-1/5"),
        Some(Drop::RightOf(k)) if k == key => Some("inset-y-0 right-0 w-1/5"),
        _ => None,
    };
    view! {
        <div
            data-tile-motion=key
            class=move || format!(
                "min-h-0 min-w-0 flex-1 md:absolute md:left-(--l) md:top-(--t) md:h-(--h) md:w-(--w) md:p-1 {} {}",
                if focused.get() && !minimized.get() { "flex" } else { "hidden" },
                if minimized.get() || (full.get() && !focused.get()) { "md:hidden" } else { "md:flex" },
            )
            style=move || { let r = rect.get(); format!("--l:{:.4}%;--t:{:.4}%;--w:{:.4}%;--h:{:.4}%", r.left * 100.0, r.top * 100.0, r.width * 100.0, r.height * 100.0) }
            node_ref=drop_area
            on:dragover=move |ev: web_sys::DragEvent| {
                if ws.dragging.get_untracked().is_some() {
                    ev.prevent_default();
                    let z = zone(&ev);
                    if ws.drop_at.get_untracked() != z {
                        ws.drop_at.set(z);
                    }
                }
            }
            on:dragleave=move |ev: web_sys::DragEvent| {
                let inside = ev.related_target().and_then(|t| t.dyn_into::<web_sys::Node>().ok())
                    .is_some_and(|t| drop_area.get_untracked().is_some_and(|el| el.contains(Some(&t))));
                if !inside && ws.drop_at.get_untracked().is_some_and(|to| to.target() == key) {
                    ws.drop_at.set(None);
                }
            }
            on:drop=move |ev: web_sys::DragEvent| {
                if let (Some(k), Some(z)) = (ws.dragging.get_untracked(), zone(&ev)) {
                    ev.prevent_default();
                    ws.move_tile(k, z);
                }
                ws.dragging.set(None);
                ws.drop_at.set(None);
            }
        >
        <section
            class=move || format!(
                "@container/tile relative flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden bg-white data-[aspect-dim=true]:opacity-30 dark:bg-zinc-950 md:rounded-lg md:border md:shadow-sm {}",
                if focused.get() && many.get() { "md:border-blue-400 md:ring-1 md:ring-blue-400/40 dark:md:border-blue-500" } else { "md:border-zinc-200 dark:md:border-zinc-800" },
            )
            on:pointerdown=move |_| ws.focus_tile_key(key)
            on:click=move |_| ws.grow(key)
        >
            {move || hint().map(|at| view! {
                <div class=format!("pointer-events-none absolute z-20 rounded-md border-2 border-blue-500 bg-blue-500/15 {at}")></div>
            })}
            // The tile's own bar: its history, its name (and a session's
            // colour, state, what runs in it, its panes and its actions),
            // and what you can do to the whole tile.
            // Dragged by its bar to another place or column. Always the
            // panel's own colours, so it reads whatever the mark; the mark
            // is an optional stripe along its top. Mouse slots relax from
            // 28 to 32 to 36px with tile width; below 30rem secondary
            // controls move into ⋯, after optional marks yield below 40rem.
            <div class="relative flex h-11 shrink-0 cursor-grab items-center border-b border-zinc-200 bg-zinc-50 text-xs active:cursor-grabbing md:@container md:px-1 dark:border-zinc-800 dark:bg-zinc-900"
                draggable=move || if many.get() { "true" } else { "false" }
                title="Drag to move this tile: onto another tile's top or bottom half, or its left or right edge for a new column"
                on:dragstart=move |ev: web_sys::DragEvent| {
                    if let Some(dt) = ev.data_transfer() {
                        let _ = dt.set_data("text/plain", &format!("tile {key}"));
                        dt.set_effect_allowed("move");
                    }
                    ws.drop_at.set(None);
                    ws.dragging.set(Some(key));
                }
                on:dragend=move |_| {
                    ws.dragging.set(None);
                    ws.drop_at.set(None);
                }
            >
                <span class="md:hidden"><crate::MobileMenu /></span>
                {move || colour.get().filter(|_| bar_colour.get() != c6_api::SessionBarColour::DotOnly).map(|c| view! { <div class="pointer-events-none absolute inset-x-0 top-0 hidden h-1 md:block" style=crate::marks::fill(&c)></div> })}
                <Show when=move || colour.get().is_some()>
                    <button class="hidden h-11 w-5 shrink-0 items-center justify-center md:flex" title="This session's mark: change it" aria-label="Its mark" on:click=move |ev| palette_at.set(Some(below(&ev, 380.0)))>
                        <crate::marks::Glyph mark=Signal::derive(move || colour.get().unwrap_or_default()) shape=crate::marks::Shape::Circle size=10 />
                    </button>
                </Show>
                // A session of your own is renamed by double-clicking its name
                // (or from ⋯).
                <div class="flex min-w-0 flex-1 items-center overflow-hidden">
                    <span node_ref=title_el class="min-w-0 truncate font-medium text-zinc-800 @min-[14rem]/tile:px-1 dark:text-zinc-100"
                        title=move || if renamable() { format!("{} · Double-click to rename", title.get()) } else { title.get() }
                        on:dblclick=move |ev| if renamable() { ev.stop_propagation(); rename_under_title() }>{title}</span>
                    <span data-tile-aspects=key class="hidden shrink-0 items-center md:@min-[40rem]/tile:flex"></span>
                </div>
                {move || chrome().map(|c| view! { <crate::session::SessionControls chrome=c /> })}
                <Show when=move || shown.get().is_some()>
                    <button class=format!("{btn} hidden md:@min-[30rem]/tile:flex")
                        disabled=move || pinnable().is_none()
                        title=move || if pinnable().is_some_and(|id| ws.pinned.with(|p| p.contains(&id))) { "Unpin" } else { "Pin to the dock" }
                        aria-label=move || if pinnable().is_some_and(|id| ws.pinned.with(|p| p.contains(&id))) { "Unpin" } else { "Pin" }
                        on:click=move |_| if let Some(id) = pinnable() { ws.toggle_pin(&id) }>
                        {move || crate::icons::star(pinnable().is_some_and(|id| ws.pinned.with(|p| p.contains(&id))))}
                    </button>
                </Show>
                <button class=btn title="Actions: session actions, history and tile controls" aria-label="Actions"
                    on:click=move |ev| menu_at.set(if menu_at.get_untracked().is_some() { None } else { Some(below(&ev, 240.0)) })>{crate::icons::ellipsis()}</button>
                <button class=format!("{btn} hidden md:@min-[14rem]/tile:flex") title=move || if full.get() { "Defocus: back to the tiles" } else { "Focus: this tile fills the screen" }
                    aria-label=move || if full.get() { "Defocus" } else { "Focus" }
                    on:click=move |ev| { ev.stop_propagation(); ws.set_full(if full.get_untracked() { None } else { Some(key) }) }>
                    {move || if full.get() { crate::icons::minimize().into_any() } else { crate::icons::maximize().into_any() }}
                </button>
                <button class=format!("{btn} hidden md:@min-[14rem]/tile:flex") title="Close this tile" aria-label="Close this tile" on:click=move |_| ws.close_tile(key)>{crate::icons::x()}</button>
                <crate::activity::SessionsPicker />
            </div>
            {move || shown.get().map(|(_, id)| view! { <crate::session::ArchivedSession id /> })}
            <Popover at=palette_at class="w-auto">
                {move || shown.get().map(|(_, id)| view! { <crate::marks::SessionPalette session=id on_pick=move |()| palette_at.set(None) /> })}
            </Popover>
            <Popover at=rename_at class="w-[min(22rem,calc(100vw-1rem))] p-3">
                // Keyed on the id, and the name read untracked: the session changes as it works, and a rebuilt form loses what was typed.
                {move || shown.get().map(|(_, id)| {
                    let title = session.get_untracked().map(|s| s.title).unwrap_or_default();
                    view! { <crate::session::RenameForm session=id title on_done=move |()| rename_at.set(None) /> }
                })}
            </Popover>
            <Popover at=menu_at class="w-60">
                {move || chrome().map(|c| view! {
                    <crate::session::SessionMenu chrome=c on_rename=move |()| rename_under_title() on_done=move |()| menu_at.set(None) />
                })}
                <div class="hidden flex-col md:flex md:@min-[30rem]/tile:hidden">
                    <Show when=move || shown.get().is_some()>
                        <button class=crate::session::MENU_ITEM disabled=move || pinnable().is_none() on:click=move |_| { if let Some(id) = pinnable() { ws.toggle_pin(&id); } menu_at.set(None); }>
                            {move || crate::icons::star(pinnable().is_some_and(|id| ws.pinned.with(|p| p.contains(&id))))}
                            {move || if pinnable().is_some_and(|id| ws.pinned.with(|p| p.contains(&id))) { "Unpin" } else { "Pin" }}
                        </button>
                    </Show>
                </div>
                <button class=format!("{} hidden md:flex md:@min-[14rem]/tile:hidden", crate::session::MENU_ITEM) on:click=move |_| { ws.set_full(if full.get_untracked() { None } else { Some(key) }); menu_at.set(None); }>
                    {move || if full.get() { crate::icons::minimize().into_any() } else { crate::icons::maximize().into_any() }}
                    {move || if full.get() { "Defocus" } else { "Focus" }}
                </button>
                <button class=format!("{} hidden md:flex md:@min-[14rem]/tile:hidden", crate::session::MENU_ITEM) on:click=move |_| { menu_at.set(None); ws.close_tile(key); }>{crate::icons::x()}"Close this tile"</button>
                <div class="flex flex-col border-t border-zinc-200 pt-1 text-xs dark:border-zinc-700">
                    <button class=crate::session::MENU_ITEM disabled=move || !can_back() on:click=move |_| { ws.step(-1); menu_at.set(None); }>{crate::icons::chevron_left()}"Back"</button>
                    <button class=crate::session::MENU_ITEM disabled=move || !can_forward() on:click=move |_| { ws.step(1); menu_at.set(None); }>{crate::icons::chevron_right()}"Forward"</button>
                    <Show when=two>
                        <button class=crate::session::MENU_ITEM on:click=move |_| { ws.detach(key); menu_at.set(None); }>{crate::icons::square_arrow_out()}"Into its own tile"</button>
                    </Show>
                    <Show when=move || many.get()>
                        <button class=crate::session::MENU_ITEM on:click=move |_| { ws.minimize(key); menu_at.set(None); }>{crate::icons::minus()}"Minimize"</button>
                    </Show>
                    <Show when=move || colour.get().is_some()>
                        <button class=crate::session::MENU_ITEM on:click=move |ev| { palette_at.set(Some(below(&ev, 380.0))); menu_at.set(None); }>{crate::icons::circle()}"Session mark"</button>
                    </Show>
                </div>
            </Popover>
            <div data-pane-host class="flex min-h-0 flex-1">
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
    fn glide_frames_never_read_geometry_or_restart_native_smooth_scrolling() {
        let motion = include_str!("../js/motion.js");
        let glide = motion.split("window.c6GlideToEnd =").nth(1).expect("the shared bounded scroll helper");
        let glide = glide.split("\n})();").next().unwrap();
        for read in ["element.scrollTop", "element.scrollHeight", "element.clientHeight"] {
            assert_eq!(glide.matches(read).count(), 1, "{read} is sampled once per invocation");
        }
        let frame = glide.split("const tick = time => {").nth(1).unwrap().split("\n    };").next().unwrap();
        for read in [".scrollTop", ".scrollHeight", ".clientHeight", ".offset", "getBoundingClientRect", "getComputedStyle"] {
            assert!(!frame.contains(read), "no {read} inside the scroll animation frame");
        }
        assert!(glide.contains("const duration = 200;"));
        assert!(!glide.contains("behavior: \"smooth\""));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn native_glide_never_invokes_a_browser_binding() {
        // A null browser handle is safe here only because native SSR is a no-op.
        let element = wasm_bindgen::JsValue::NULL.unchecked_into::<web_sys::Element>();
        glide_to_end(&element);
        stop_glide_to_end(&element);
    }

    #[test]
    fn motion_plan_tracks_geometry_and_panes_but_not_active_chrome() {
        let mut layout = Layout::new(View::Sessions);
        layout.open_tile(View::Workers);
        let original = motion_layout(&layout);
        let first = layout.tiles[0].key;
        let col = layout.tiles[0].columns[0].key;
        layout.tiles[0].active = 0;
        layout.tiles[0].columns[0].view = View::Inbox;
        assert!(original == motion_layout(&layout), "view/chrome updates do not measure or interrupt motion");
        layout.open_after(first, col, View::Decisions); // a new destination, not the already-open Workers
        assert!(original != motion_layout(&layout), "pane additions have their own FLIP boundary");
        assert_eq!(motion_layout(&layout)[0].panes.len(), 2);
        let split = motion_layout(&layout);
        layout.tiles[0].cursor = 0;
        assert!(split != motion_layout(&layout), "phone pane visibility changes animate too");
    }

    #[test]
    fn motion_plan_matches_maximize_minimize_restore_and_phone_visibility() {
        let mut layout = Layout::new(View::Sessions);
        layout.open_tile(View::Workers);
        let plan = motion_layout(&layout);
        assert!(plan.iter().all(|t| t.desktop));
        assert_eq!(plan.iter().filter(|t| t.phone).count(), 1);
        assert!(plan[1].phone);
        layout.full = true;
        let plan = motion_layout(&layout);
        assert!(!plan[0].desktop);
        assert!(plan[1].desktop);
        assert_eq!(plan[1].rect, [0.0, 0.0, 1.0, 1.0]);
        layout.full = false;
        layout.minimize(1);
        let plan = motion_layout(&layout);
        assert!(!plan[1].desktop && !plan[1].phone);
        assert_eq!(plan[0].rect, [0.0, 0.0, 1.0, 1.0]);
        layout.show(1);
        assert!(motion_layout(&layout).iter().all(|t| t.desktop));
    }

    #[test]
    fn closing_the_last_tile_gives_the_live_home_a_new_motion_identity() {
        let mut layout = Layout::new(View::Sessions);
        let (tile, pane) = (layout.tiles[0].key, layout.tiles[0].columns[0].key);
        layout.full = true;
        layout.remove_tile(0);
        assert_ne!(layout.tiles[0].key, tile);
        assert_ne!(layout.tiles[0].columns[0].key, pane);
        assert_eq!(layout.tiles[0].columns[0].view, View::home());
        assert_eq!(layout.tiles[0].stack, Some(0));
        assert!(!layout.full);
        assert!(layout.valid());
    }

    #[test]
    fn initial_loading_prioritizes_focus_without_mounting_hidden_panes() {
        for shown in [false, true] {
            for focused in [false, true] {
                for released in [false, true] {
                    assert_eq!(initial_mount(false, shown, focused, released), shown && (focused || released));
                    assert!(initial_mount(true, shown, focused, released), "hiding must never dispose a mounted pane");
                }
            }
        }
    }

    #[test]
    fn open_session_titles_status_and_marks_do_not_depend_on_the_live_list() {
        Owner::new().with(|| {
            let archived: Session = serde_json::from_value(serde_json::json!({
                "id": "old", "worker_id": "00000000-0000-0000-0000-000000000001", "title": "Saved title",
                "cwd": "", "connection_id": "c", "model": "m", "status": "idle", "archived": true,
                "colour": "navy/dots", "error": null, "created_at": 0, "updated_at": 0, "last_seq": 0
            }))
            .unwrap();
            let (sessions, set_sessions) = signal(Vec::new());
            let live = LiveFeed {
                workers: signal(Vec::new()).0,
                sessions,
                resolved_sessions: RwSignal::new(HashMap::from([("old".to_owned(), archived.clone())])),
                connected: signal(true).0,
                doc: signal(None).0,
                marks: signal(Default::default()).0,
                attention: signal(0).0,
            };
            assert_eq!(View::session("old").label(&live), "Saved title");
            assert_eq!(View::SessionChanges { id: "old".into() }.label(&live), "Changes · Saved title");
            assert!(live.sessions.get().is_empty(), "resolved rows never enter the rail");
            let resolved = live.session("old").unwrap();
            assert!(resolved.archived);
            assert_eq!(resolved.status, SessionStatus::Idle);
            assert_eq!(crate::marks::of(&resolved), "navy/dots");
            let mut active = archived.clone();
            active.archived = false;
            active.title = "Restored title".into();
            active.status = SessionStatus::Running;
            set_sessions.set(vec![active.clone()]);
            assert_eq!(live.session("old"), Some(active), "the fresh live row wins after unarchive");
            assert_eq!(View::session("old").label(&live), "Restored title");
            for view in [View::session("old"), View::SessionFiles { id: "old".into() }, View::SessionTerminal { id: "old".into() }] {
                assert_eq!(view.session_id(), Some("old"));
            }
            assert_eq!(View::Sessions.session_id(), None);
        });
    }

    #[test]
    fn background_loading_waits_for_every_shown_focused_pane() {
        let mut l = Layout::new(View::Sessions);
        let tile = l.tiles[0].key;
        let list = l.tiles[0].columns[0].key;
        let session = l.key();
        l.tiles[0].columns.push(Column { key: session, view: View::session("s1") });
        l.tiles[0].settle();
        let mut drawn = HashSet::from([list]);
        assert_eq!(focused_drawn(&l, false, &drawn), None, "a list shell cannot release its loading session");
        assert_eq!(focused_drawn(&l, true, &drawn), None, "the phone shows the session, not the list");
        drawn.insert(session);
        assert_eq!(focused_drawn(&l, false, &drawn), Some(vec![list, session]));
        assert_eq!(focused_drawn(&l, true, &HashSet::from([session])), Some(vec![session]));
        l.tiles[0].minimized = true;
        assert_eq!(focused_drawn(&l, false, &drawn), None);
        l.tiles[0].minimized = false;
        assert!(l.pane_shown(tile, list, false));
    }

    #[test]
    fn header_metadata_waits_with_hidden_panes_then_resolves_every_open_session() {
        let mut l = Layout::new(View::session("first"));
        l.open_tile(View::session("focus"));
        let side = l.key();
        l.tiles[1].columns.push(Column { key: side, view: View::session("side") });
        l.tiles[1].settle();
        l.open_tile(View::session("minimized"));
        l.tiles[2].minimized = true;
        l.focus = 1;
        let ids = |ss: &[&str]| ss.iter().map(|s| s.to_string()).collect::<HashSet<_>>();
        assert_eq!(initial_metadata_ids(&l, false, false), ids(&["focus", "side"]));
        assert_eq!(initial_metadata_ids(&l, true, false), ids(&["side"]), "a hidden phone pane does not start a header read");
        for phone in [false, true] {
            assert_eq!(
                initial_metadata_ids(&l, phone, true),
                ids(&["first", "focus", "side", "minimized"]),
                "after the focused paint, omitted/archived headers resolve even for minimized tiles"
            );
        }
        l.focus = 0;
        assert_eq!(initial_metadata_ids(&l, true, false), ids(&["first"]), "changing focus immediately changes eligibility");
    }

    #[test]
    fn changing_focus_or_closing_the_loading_tile_cannot_release_stale_readiness() {
        let mut l = Layout::new(View::session("s1"));
        let first = l.tiles[0].columns[0].key;
        let second = l.new_tile(View::session("s2"));
        let other = second.columns[0].key;
        l.tiles.push(second);
        let drawn = HashSet::from([first]);
        assert_eq!(focused_drawn(&l, false, &drawn), Some(vec![first]));
        l.focus = 1;
        assert_eq!(focused_drawn(&l, false, &drawn), None, "focus must draw before the barrier opens");
        assert_eq!(focused_drawn(&l, false, &HashSet::from([other])), Some(vec![other]));
        l.tiles.remove(0);
        l.focus = 0;
        assert_eq!(focused_drawn(&l, false, &drawn), None, "a closed tile's replay is irrelevant");
    }

    #[test]
    fn upward_input_stops_following_before_any_scroll_and_even_one_pixel_from_end() {
        assert!(!scroll_follow(true, Some(ScrollIntent::Away), None));
        for gap in [120, 40, 4, 3, 1, 0, -1] {
            assert!(!scroll_follow(false, Some(ScrollIntent::Away), Some(gap)), "gap {gap}");
        }
    }

    #[test]
    fn programmatic_scrolls_never_change_following() {
        for following in [false, true] {
            for gap in [400, 119, 3, 0, -1] {
                assert_eq!(scroll_follow(following, None, Some(gap)), following, "gap {gap}");
            }
        }
    }

    #[test]
    fn latest_can_resume_before_the_old_upward_intent_is_cleared() {
        assert!(!scroll_follow(true, Some(ScrollIntent::Away), None));
        // Latest explicitly sets following. Its own scroll event must not
        // replay the earlier upward input while the reactive effect is pending.
        assert!(scroll_follow(true, Some(ScrollIntent::Away), Some(0)));
    }

    #[test]
    fn downward_input_resumes_only_after_reaching_the_very_end() {
        assert!(!scroll_follow(false, Some(ScrollIntent::Toward), None));
        for gap in [120, 40, 4] {
            assert!(!scroll_follow(false, Some(ScrollIntent::Toward), Some(gap)), "gap {gap}");
        }
        for gap in [3, 1, 0, -1] {
            assert!(scroll_follow(false, Some(ScrollIntent::Toward), Some(gap)), "gap {gap}");
        }
        // Content growth while following is not a reason to stop.
        assert!(scroll_follow(true, Some(ScrollIntent::Toward), Some(400)));
    }

    #[test]
    fn scrollbar_follows_only_at_the_very_end() {
        assert!(scroll_follow(true, Some(ScrollIntent::Scrollbar), None));
        assert!(!scroll_follow(true, Some(ScrollIntent::Scrollbar), Some(4)));
        assert!(scroll_follow(false, Some(ScrollIntent::Scrollbar), Some(3)));
    }

    #[test]
    fn wheel_and_touch_direction_ignore_zero_movement() {
        assert_eq!(ScrollIntent::from_delta(-0.5), Some(ScrollIntent::Away));
        assert_eq!(ScrollIntent::from_delta(0.5), Some(ScrollIntent::Toward));
        assert_eq!(ScrollIntent::from_delta(0.0), None);
        // Finger down scrolls up; finger up scrolls down.
        assert_eq!(ScrollIntent::from_delta(100.0 - 101.0), Some(ScrollIntent::Away));
        assert_eq!(ScrollIntent::from_delta(100.0 - 99.0), Some(ScrollIntent::Toward));
    }

    #[test]
    fn scroll_keys_have_direction_and_shift_space_scrolls_up() {
        for key in ["ArrowUp", "PageUp", "Home"] {
            assert_eq!(ScrollIntent::from_key(key, false), Some(ScrollIntent::Away), "{key}");
        }
        for key in ["ArrowDown", "PageDown", "End", " "] {
            assert_eq!(ScrollIntent::from_key(key, false), Some(ScrollIntent::Toward), "{key}");
        }
        assert_eq!(ScrollIntent::from_key(" ", true), Some(ScrollIntent::Away));
        assert_eq!(ScrollIntent::from_key("a", false), None);
        assert_eq!(ScrollIntent::from_key("Tab", false), None);
    }

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
        let key = layout.key();
        layout.tiles[0].columns.push(Column { key, view: View::Brief });
        layout.open_tile(View::Calendar);
        for _ in 0..2 {
            layout.open(None, View::Brief, How::OwnTile);
            assert_eq!(layout.tiles.len(), 2);
            assert_eq!(layout.focus, 0);
            assert_eq!(layout.tiles[0].cursor, 1);
            assert_eq!(layout.tiles[0].active, key);
            assert_eq!(layout.tiles[0].columns[1].key, key);
            assert_eq!(layout.tiles[0].columns[1].view, View::Brief);
        }
    }

    fn destinations() -> Vec<View> {
        // Every view: exact repeats are single-copy, in either pane. The
        // identities with navigation state are covered separately below.
        [
            r#"{"view":"sessions"}"#,
            r#"{"view":"session","id":"s"}"#,
            r#"{"view":"session_changes","id":"s"}"#,
            r#"{"view":"session_terminal","id":"s"}"#,
            r#"{"view":"session_files","id":"s"}"#,
            r#"{"view":"session_outputs","id":"s"}"#,
            r#"{"view":"search","q":"needle"}"#,
            r#"{"view":"workers"}"#,
            r#"{"view":"worker","id":"w"}"#,
            r#"{"view":"settings"}"#,
            r#"{"view":"inbox"}"#,
            r#"{"view":"mission"}"#,
            r#"{"view":"usage"}"#,
            r#"{"view":"chats"}"#,
            r#"{"view":"analytics"}"#,
            r#"{"view":"activity"}"#,
            r#"{"view":"memex"}"#,
            r#"{"view":"mail"}"#,
            r#"{"view":"chat"}"#,
            r#"{"view":"conversation","target":"mail:m"}"#,
            r#"{"view":"brief"}"#,
            r#"{"view":"calendar"}"#,
            r#"{"view":"contacts"}"#,
            r#"{"view":"photos"}"#,
            r#"{"view":"decisions"}"#,
            r#"{"view":"work"}"#,
            r#"{"view":"work","id":"w"}"#,
            r#"{"view":"projects"}"#,
            r#"{"view":"projects","id":"p"}"#,
            r#"{"view":"docs"}"#,
            r#"{"view":"docs","id":"d"}"#,
            r#"{"view":"collections"}"#,
            r#"{"view":"collections","name":"c"}"#,
            r#"{"view":"realms"}"#,
            r#"{"view":"explore","realm":"home","section":"docs"}"#,
            r#"{"view":"app","name":"a"}"#,
            r#"{"view":"app","name":"a","record":"r"}"#,
        ]
        .into_iter()
        .map(|json| serde_json::from_str(json).unwrap())
        .collect()
    }

    #[test]
    fn opening_any_existing_destination_reveals_its_exact_pane_without_history_or_key_churn() {
        for view in destinations() {
            for pane in 0..2 {
                for how in [How::Beside, How::OwnTile, How::Instead] {
                    let mut l = Layout::new(view.clone());
                    let extra = Column { key: l.key(), view: View::Worker { id: "different".into() } };
                    l.tiles[0].columns.insert(1 - pane, extra);
                    l.tiles[0].cursor = 1 - pane;
                    let columns = l.tiles[0].columns.clone();
                    let target_key = columns[pane].key;
                    l.open_tile(View::Worker { id: "origin".into() });
                    let from = Some((l.tiles[1].key, l.tiles[1].columns[0].key));
                    l.tiles[0].minimized = true;
                    l.full = true; // another tile is maximized
                    let next = l.next;
                    l.open(from, view.clone(), how);
                    assert_eq!(l.tiles.len(), 2, "{view:?}, {how:?}");
                    assert_eq!(l.next, next + 1, "only the MRU counter advances; no pane or tile keys allocated");
                    assert_eq!(l.focus, 0);
                    assert!(!l.tiles[0].minimized);
                    assert_eq!(l.tiles[0].columns, columns);
                    assert_eq!(l.tiles[0].cursor, pane);
                    assert_eq!(l.tiles[0].active, target_key);
                    assert!(l.pane_shown(l.tiles[0].key, target_key, true), "the target is shown even in focus/phone mode");
                    assert!(l.tiles[0].back.is_empty());
                    assert!(l.tiles[0].forward.is_empty());
                }
            }
        }
    }

    #[test]
    fn explicit_new_tile_allows_every_destination_including_sessions_and_docs() {
        for view in destinations() {
            let mut l = Layout::new(view.clone());
            l.place();
            let original = l.tiles[0].clone();
            l.open(Some((original.key, original.columns[0].key)), view.clone(), How::NewTile);
            assert_eq!(l.tiles.len(), 2, "{view:?}");
            assert_eq!(l.focus, 1);
            assert_eq!(l.tiles[0], original);
            assert_eq!(l.focused_view(), Some(&view));
            assert_ne!(l.tiles[0].columns[0].key, l.tiles[1].columns[0].key);
        }
    }

    #[test]
    fn repeated_document_auto_opens_from_two_sessions_reuse_either_pane_and_keep_history() {
        for pane in 0..2 {
            let doc = View::from_path("/docs/shared#block.2");
            let mut l = Layout::new(View::session("one"));
            l.open_tile(View::session("two"));
            let sessions = l.tiles.clone();
            let from = sessions.iter().map(|t| (t.key, t.columns[0].key)).collect::<Vec<_>>();
            l.open(Some(from[0]), View::from_path("/docs/shared#block.1"), How::OwnTile);
            l.open(Some(from[0]), doc.clone(), How::OwnTile);
            let extra = Column { key: l.key(), view: View::Inbox };
            l.tiles[2].columns.insert(1 - pane, extra);
            l.tiles[2].settle();
            let target = l.tiles[2].clone();
            for source in from.iter().cycle().take(6) {
                l.tiles[2].minimized = true;
                l.focus = 0;
                l.full = true;
                // SessionView uses an untargeted OwnTile open on creation;
                // subsequent edits refresh only (covered by session tests).
                l.open(Some(*source), View::from_path("/docs/shared"), How::OwnTile);
                assert_eq!(l.tiles.len(), 3);
                assert_eq!(l.focus, 2);
                assert!(!l.tiles[2].minimized);
                assert_eq!(l.tiles[2].columns, target.columns);
                assert_eq!(l.tiles[2].active, target.columns[pane].key);
                assert_eq!(l.tiles[2].back, target.back);
                assert_eq!(l.tiles[2].forward, target.forward);
                assert_eq!(&l.tiles[..2], sessions.as_slice(), "neither session is replaced");
                assert_eq!(l.focused_view(), Some(&doc), "an edit must not reset the reading target");
            }
        }
    }

    #[test]
    fn session_and_doc_targets_reuse_keys_and_record_back_forward_without_resetting_on_plain_links() {
        for path in ["/sessions/s?q=needle&open=child#turn-4", "/docs/d#block.2"] {
            let targeted = View::from_path(path);
            let plain = View::from_path(path.split(['?', '#']).next().unwrap());
            let previous = View::from_path(&match &plain {
                View::Docs { .. } => "/docs/d?block=block.1".to_owned(),
                _ => "/sessions/s#turn-1".to_owned(),
            });
            let mut l = Layout::new(View::Sessions);
            let from = (l.tiles[0].key, l.tiles[0].columns[0].key);
            l.open(Some(from), previous.clone(), How::Beside);
            let columns = l.tiles[0].columns.clone();
            let key = columns[1].key;
            l.open_tile(View::Inbox);
            let origin = l.tiles[1].clone();
            l.open(None, targeted.clone(), How::OwnTile);
            assert_eq!(l.tiles[0].columns[1].key, key);
            assert_eq!(l.tiles[0].back.last(), Some(&columns));
            assert_eq!(l.focused_view(), Some(&targeted));
            assert_eq!(l.tiles[1], origin, "the source tile/history is untouched");
            let history = l.tiles[0].back.clone();
            for view in [targeted.clone(), plain.clone()] {
                l.open(None, view, How::Beside);
                assert_eq!(l.tiles[0].back, history);
                assert_eq!(l.focused_view(), Some(&targeted));
            }
            l.step(-1);
            assert_eq!(l.focused_view(), Some(&previous));
            assert_eq!(l.tiles[0].columns[1].key, key);
            let forward = l.tiles[0].forward.clone();
            l.open(None, plain, How::OwnTile);
            assert_eq!(l.tiles[0].forward, forward, "focus doesn't discard Forward");
            l.step(1);
            assert_eq!(l.focused_view(), Some(&targeted));
            l.step(-1);
            l.open(None, View::from_path(&format!("{}#different", path.split(['?', '#']).next().unwrap())), How::Beside);
            assert!(l.tiles[0].forward.is_empty(), "a new target forks history");
        }
    }

    #[test]
    fn restored_session_duplicates_are_not_collapsed_and_origin_or_focus_wins() {
        let view = View::session("s");
        let mut saved = Layout::new(view.clone());
        saved.open(None, view.clone(), How::NewTile);
        saved.open(None, View::Inbox, How::NewTile);
        let mut l: Layout = serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();
        l.restore_documents();
        assert_eq!(l, saved, "restoration keeps intentional and old duplicates");
        let keys: Vec<_> = l.tiles.iter().map(|t| t.key).collect();
        let from = (l.tiles[1].key, l.tiles[1].columns[0].key);
        l.open(Some(from), view.clone(), How::Beside);
        assert_eq!(l.focus, 1, "a link from a copy stays in that copy");
        l.open(None, view.clone(), How::OwnTile);
        assert_eq!(l.focus, 1, "external navigation prefers the focused copy");
        l.focus = 2;
        l.open(None, view, How::Beside);
        assert_eq!(l.focus, 0, "otherwise the first live match wins deterministically");
        assert_eq!(l.tiles.iter().map(|t| t.key).collect::<Vec<_>>(), keys);
    }

    #[test]
    fn restore_collapses_document_copies_to_focused_or_first_without_losing_other_panes() {
        for focus in 0..3 {
            let first = View::from_path("/docs/d#first");
            let second = View::from_path("/docs/d#second");
            let mut saved = Layout::new(first.clone());
            let inbox = Column { key: saved.key(), view: View::Inbox };
            saved.tiles[0].columns.push(inbox.clone());
            saved.tiles[0].settle();
            saved.tiles[0].back.push(vec![Column { key: 100, view: View::Decisions }]);
            saved.tiles[0].forward.push(vec![Column { key: 101, view: View::Workers }]);
            saved.open(None, second.clone(), How::NewTile);
            saved.open_tile(View::session("s"));
            saved.focus = focus;
            saved.full = true;
            let winner = if focus == 1 { 1 } else { 0 };
            let winner_column = saved.tiles[winner].columns[0].clone();
            let focused = saved.tiles[focus].key;
            let histories = (saved.tiles[0].back.clone(), saved.tiles[0].forward.clone());
            let mut l: Layout = serde_json::from_str(&serde_json::to_string(&saved).unwrap()).unwrap();
            l.restore_documents();
            let docs: Vec<_> =
                l.tiles.iter().flat_map(|t| &t.columns).filter(|c| matches!(&c.view, View::Docs { id, .. } if id == "d")).collect();
            assert_eq!(docs, [&winner_column]);
            assert_eq!(l.tiles[l.focus].key, focused);
            assert!(l.full);
            let mixed = l.tile(saved.tiles[0].key).unwrap();
            assert!(mixed.columns.contains(&inbox));
            assert_eq!((&mixed.back, &mixed.forward), (&histories.0, &histories.1));
            assert_eq!(mixed.active, inbox.key, "a surviving companion keeps focus");
            assert!(l.valid());
            let normalized = l.clone();
            l.restore_documents();
            assert_eq!(l, normalized, "normalization is idempotent");
        }
    }

    #[test]
    fn restore_prefers_active_document_within_focused_tile_but_leaves_doc_lists_and_sessions() {
        for active in 0..2 {
            let mut l = Layout::new(View::from_path("/docs/d#first"));
            let copy = Column { key: l.key(), view: View::from_path("/docs/d#second") };
            l.tiles[0].columns.push(copy);
            l.tiles[0].cursor = active;
            l.tiles[0].active = l.tiles[0].columns[active].key;
            let winner = l.tiles[0].columns[active].clone();
            let index = View::from_path("/docs");
            for v in [index.clone(), index, View::session("s"), View::session("s")] {
                l.open_tile(v);
            }
            l.focus = 0;
            let rest = l.tiles[1..].to_vec();
            l.restore_documents();
            assert_eq!(l.tiles[0].columns, [winner]);
            assert_eq!(l.tiles[1..], rest);
            assert!(l.valid());
        }
    }

    #[test]
    fn restore_duplicate_only_tiles_traverses_history_without_discarding_either_stack() {
        for back in [true, false] {
            let mut l = Layout::new(View::from_path("/docs/d#keep"));
            l.open_tile(View::from_path("/docs/d#lose"));
            let original = l.tiles[1].columns.clone();
            let skipped = vec![Column { key: l.key(), view: View::from_path("/docs/d#older") }];
            let useful = vec![Column { key: l.key(), view: View::Inbox }];
            let opposite = vec![Column { key: l.key(), view: View::Workers }];
            if back {
                l.tiles[1].back = vec![useful.clone(), skipped.clone()];
            } else {
                l.tiles[1].forward = vec![useful.clone(), skipped.clone()];
            }
            // The opposite direction also remains intact. Prefer Back when
            // both have a useful page; test Forward with Back empty.
            if back {
                l.tiles[1].forward = vec![opposite.clone()];
            }
            l.focus = 0;
            l.restore_documents();
            assert_eq!(l.tiles[1].columns, useful);
            let (visited, returned) = if back { (&l.tiles[1].back, &l.tiles[1].forward) } else { (&l.tiles[1].forward, &l.tiles[1].back) };
            assert!(visited.is_empty());
            let mut expected = if back { vec![opposite] } else { Vec::new() };
            expected.extend([original, skipped]);
            assert_eq!(*returned, expected);
            assert!(l.valid());
            l.focus = 1;
            let before = l.tiles[1].clone();
            l.step(if back { 1 } else { -1 });
            assert_eq!(l.focus, 0, "revisiting the removed doc reveals its survivor");
            assert_eq!(l.tiles[1], before, "no duplicate is recreated or history consumed");
            assert_eq!(l.focused_view(), Some(&View::from_path("/docs/d#older")));
        }
    }

    #[test]
    fn restore_keeps_duplicate_only_navigation_as_home_instead_of_merging_or_deleting_it() {
        let mut l = Layout::new(View::from_path("/docs/d#keep"));
        l.open_tile(View::from_path("/docs/d#lose"));
        let previous = vec![Column { key: l.key(), view: View::from_path("/docs/d#previous") }];
        let future = vec![Column { key: l.key(), view: View::from_path("/docs/d#future") }];
        l.tiles[1].back.push(previous.clone());
        l.tiles[1].forward.push(future.clone());
        let next = l.next;
        let source = l.tiles[1].clone();
        l.focus = 0;
        l.restore_documents();
        assert_eq!(l.tiles[1].columns[0].view, View::home());
        assert_eq!(l.tiles[1].back, [previous]);
        assert_eq!(l.tiles[1].forward, [future, source.columns]);
        assert_eq!(l.tiles[1].columns[0].key, next + 1, "the Home shell allocates exactly one fresh pane key");
        assert_eq!(l.next, next + 1);
        let normalized = l.clone();
        l.restore_documents();
        assert_eq!(l, normalized);
        l.focus = 1;
        let source = l.tiles[1].clone();
        l.step(-1);
        assert_eq!(l.tiles[1], source);
        assert_eq!(l.focused_view(), Some(&View::from_path("/docs/d#previous")));
    }

    #[test]
    fn restore_reviving_history_claims_new_documents_before_retaining_another_copy() {
        let mut l = Layout::new(View::from_path("/docs/d#keep"));
        l.open_tile(View::from_path("/docs/d#lose"));
        let first = Column { key: l.key(), view: View::from_path("/docs/x#first") };
        let second = Column { key: l.key(), view: View::from_path("/docs/x#second") };
        l.tiles[1].back.push(vec![first.clone(), second]);
        l.focus = 0;
        l.restore_documents();
        assert_eq!(l.tiles[1].columns, [first]);
        assert!(l.valid());
        let normalized = l.clone();
        l.restore_documents();
        assert_eq!(l, normalized, "revived document copies must collapse in the first pass");
    }

    #[test]
    fn history_reveals_live_documents_without_discarding_source_panes_or_history() {
        for delta in [-1, 1] {
            let mut l = Layout::new(View::session("source"));
            let snapshot =
                vec![Column { key: l.key(), view: View::Inbox }, Column { key: l.key(), view: View::from_path("/docs/d#target") }];
            if delta < 0 {
                l.tiles[0].back.push(snapshot);
            } else {
                l.tiles[0].forward.push(snapshot);
            }
            l.open_tile(View::from_path("/docs/d#reading"));
            l.tiles[1].minimized = true;
            l.focus = 0;
            l.full = true;
            let source = l.tiles[0].clone();
            l.step(delta);
            assert_eq!(l.tiles[0], source, "a mixed history snapshot stays intact and available");
            assert_eq!(l.focus, 1);
            assert!(!l.tiles[1].minimized);
            assert_eq!(l.focused_view(), Some(&View::from_path("/docs/d#target")));
            assert_eq!(l.tiles.len(), 2);
        }
    }

    #[test]
    fn history_only_doc_copies_within_one_snapshot_do_not_become_two_live_panes() {
        let mut l = Layout::new(View::Inbox);
        let snapshot = vec![
            Column { key: l.key(), view: View::from_path("/docs/d#first") },
            Column { key: l.key(), view: View::from_path("/docs/d#second") },
        ];
        l.tiles[0].back.push(snapshot.clone());
        let original = l.tiles[0].columns.clone();
        l.step(-1);
        assert_eq!(l.tiles[0].columns, [snapshot[0].clone()]);
        assert_eq!(l.tiles[0].forward.as_slice(), std::slice::from_ref(&original));
        assert!(l.valid());
        l.step(1);
        assert_eq!(l.tiles[0].columns, original);
    }

    #[test]
    fn target_history_in_the_left_pane_keeps_that_pane_active() {
        for path in ["/sessions/s#e1", "/docs/d#block.1"] {
            let first = View::from_path(path);
            let second = View::from_path(&format!("{}#next", path.split('#').next().unwrap()));
            let mut l = Layout::new(first.clone());
            let right = Column { key: l.key(), view: View::Inbox };
            l.tiles[0].columns.push(right);
            l.tiles[0].settle();
            l.open(None, second.clone(), How::Beside);
            assert_eq!(l.focused_view(), Some(&second));
            l.step(-1);
            assert_eq!(l.focused_view(), Some(&first));
            assert_eq!(l.tiles[0].cursor, 0);
            l.step(1);
            assert_eq!(l.focused_view(), Some(&second));
            assert_eq!(l.tiles[0].cursor, 0);
        }
    }

    #[test]
    fn history_only_matches_do_not_steal_a_live_pane_or_modify_old_history() {
        let doc = View::Docs { id: "d".into(), target: None };
        let mut l = Layout::new(doc.clone());
        l.open(None, View::Inbox, How::Instead);
        let history = l.tiles[0].back.clone();
        l.open(None, doc.clone(), How::OwnTile);
        assert_eq!(l.tiles.len(), 2);
        assert_eq!(l.tiles[0].back, history);
        assert_eq!(l.tiles[0].columns[0].view, View::Inbox);
        assert_eq!(l.focused_view(), Some(&doc));
    }

    #[test]
    fn selected_parent_opens_reuse_general_destinations_but_keep_new_session_companions_beside_the_parent() {
        for view in [View::Worker { id: "worker".into() }, View::session("started"), View::Docs { id: "d".into(), target: None }] {
            let mut l = Layout::new(View::Sessions);
            let selected = Column { key: l.key(), view: View::session("parent") };
            l.tiles[0].columns.push(selected.clone());
            l.open_tile(view.clone());
            let parent = l.tiles[0].key;
            let before = l.tiles[0].clone();
            l.tiles[1].minimized = true;
            l.focus = 0;
            l.open_after(parent, selected.key, view);
            assert_eq!(l.tiles.len(), 2);
            assert_eq!(l.focus, 1);
            assert!(!l.tiles[1].minimized);
            assert_eq!(l.tiles[0], before, "existing detail destination leaves its source intact");
            l.open_after(parent, selected.key, View::SessionChanges { id: "parent".into() });
            assert_eq!(l.tiles[0].columns[0], selected, "new companion keeps the selected transcript, not the list");
            assert_eq!(l.tiles[0].columns[1].view, View::SessionChanges { id: "parent".into() });
        }
    }

    #[test]
    fn settings_sections_and_output_selection_navigate_one_destination_but_different_queries_and_objects_do_not() {
        for (a, b) in [("/settings/appearance", "/settings/models"), ("/sessions/s/outputs?output=a", "/sessions/s/outputs?output=b")] {
            let (a, b) = (View::from_path(a), View::from_path(b));
            let mut l = Layout::new(a.clone());
            let key = l.tiles[0].columns[0].key;
            l.open(None, b.clone(), How::OwnTile);
            assert_eq!(l.tiles.len(), 1);
            assert_eq!(l.focused_view(), Some(&b));
            assert_eq!(l.tiles[0].columns[0].key, key);
            l.step(-1);
            assert_eq!(l.focused_view(), Some(&a));
        }
        for (a, b) in [
            ("/search?q=a", "/search?q=b"),
            ("/search?q=a&kind=doc", "/search?q=a&kind=session"),
            ("/docs/a", "/docs/b"),
            ("/docs", "/docs/a"),
            ("/work/a", "/work/b"),
            ("/projects/a", "/projects/b"),
            ("/collections/a", "/collections/b"),
            ("/workers/a", "/workers/b"),
            ("/sessions/a", "/sessions/b"),
        ] {
            let mut l = Layout::new(View::from_path(a));
            l.open(None, View::from_path(b), How::OwnTile);
            assert_eq!(l.tiles.len(), 2, "{a} and {b} are distinct destinations");
        }
    }

    #[test]
    fn new_destinations_keep_beside_instead_and_own_tile_history_rules() {
        for how in [How::Beside, How::Instead, How::OwnTile, How::NewTile] {
            let mut l = Layout::new(View::Sessions);
            let original = l.tiles[0].columns.clone();
            let from = (l.tiles[0].key, original[0].key);
            let view = View::Docs { id: "new".into(), target: Some("b".into()) };
            l.open(Some(from), view.clone(), how);
            assert_eq!(l.focused_view(), Some(&view));
            if matches!(how, How::OwnTile | How::NewTile) {
                assert_eq!(l.tiles.len(), 2);
                assert!(l.tiles[0].back.is_empty());
            } else {
                assert_eq!(l.tiles.len(), 1);
                assert_eq!(l.tiles[0].columns.len(), if how == How::Beside { 2 } else { 1 });
                assert_eq!(l.tiles[0].back, vec![original]);
                l.step(-1);
                assert_eq!(l.focused_view(), Some(&View::Sessions));
                l.step(1);
                assert_eq!(l.focused_view(), Some(&view));
            }
        }
    }

    #[test]
    fn document_and_session_links_preserve_hashes_and_roundtrip_targets() {
        for path in ["/docs/d#block.2", "/docs/d?block=block.2", "/docs/d?block=ignored#block.2"] {
            let view = View::from_path(path);
            assert_eq!(view, View::Docs { id: "d".into(), target: Some("block.2".into()) });
            assert_eq!(view.path(), "/docs/d#block.2");
        }
        for path in ["/docs/d#block%20%C3%A9%2B", "/sessions/s?q=two+words&open=child%2Cnext#turn%2B4"] {
            let view = View::from_path(path);
            assert_eq!(View::from_path(&view.path()), view);
        }
        assert_eq!(View::from_path("/docs/d#block+2"), View::Docs { id: "d".into(), target: Some("block+2".into()) });
        assert_eq!(
            View::from_path("/sessions/s#turn-4"),
            View::Session { id: "s".into(), target: Some(Target { anchor: Some("turn-4".into()), ..Default::default() }) }
        );
        assert_eq!(
            View::from_path("/sessions/s?open=child"),
            View::Session { id: "s".into(), target: Some(Target { open: vec!["child".into()], ..Default::default() }) }
        );
        assert_eq!(serde_json::from_str::<View>(r#"{"view":"docs","id":"old"}"#).unwrap(), View::Docs { id: "old".into(), target: None });
    }

    #[test]
    fn mounted_content_keys_retain_documents_but_replace_targeted_sessions() {
        let doc = View::from_path("/docs/d");
        let doc_target = View::from_path("/docs/d#block.1");
        assert_eq!(content_key(doc.clone(), 0), content_key(doc_target.clone(), 1), "a doc target must retain its editor owner");
        assert_eq!(content_key(doc_target.clone(), 1), content_key(doc_target, 2), "revisiting a block must not remount DocPage");
        assert_ne!(content_key(doc, 0), content_key(View::from_path("/docs/other"), 0));
        let session = View::from_path("/sessions/s");
        let target = View::from_path("/sessions/s?q=needle#e20");
        assert_ne!(content_key(session.clone(), 0), content_key(target.clone(), 1), "a session target needs a fresh owner and DOM");
        assert_ne!(content_key(target.clone(), 1), content_key(target, 2), "a repeated target needs the same clean remount boundary");
        assert_eq!(content_key(session.clone(), 1), content_key(session, 2), "ordinary focus must retain the transcript");
    }

    #[test]
    fn a_sessions_panes_have_their_own_addresses() {
        use crate::panes::Pane;
        for (view, path, pane) in [
            (View::SessionChanges { id: "s1".into() }, "/sessions/s1/changes", Pane::Changes),
            (View::SessionTerminal { id: "s1".into() }, "/sessions/s1/terminal", Pane::Terminal),
            (View::SessionFiles { id: "s1".into() }, "/sessions/s1/files", Pane::Files),
            (View::SessionOutputs { id: "s1".into(), output: String::new() }, "/sessions/s1/outputs", Pane::Outputs),
        ] {
            assert_eq!(view.path(), path);
            assert_eq!(view.session_pane(), Some((pane, "s1")));
            assert!(!view.is_section(), "an item: it opens beside, not in place of a list");
            let back: View = serde_json::from_str(&serde_json::to_string(&view).unwrap()).unwrap();
            assert_eq!(back, view, "kept with the layout");
        }
        // Saved before an output was picked.
        let v: View = serde_json::from_str(r#"{"view":"session_outputs","id":"s1"}"#).unwrap();
        assert_eq!(v, View::SessionOutputs { id: "s1".into(), output: String::new() });
        assert_eq!(View::session("s1").session_pane(), None);
    }

    #[test]
    fn a_started_session_opens_beside_the_card_and_is_shown_on_a_phone() {
        for source in [View::session("proposer"), View::Decisions] {
            // The card can be in the item beside a list, not the first pane.
            let mut l = Layout::new(View::Sessions);
            let tile = l.tiles[0].key;
            let card = l.key();
            l.tiles[0].columns.push(Column { key: card, view: source.clone() });
            l.tiles[0].settle();
            // Async completion must open beside the card, even if focus moved.
            l.open_tile(View::Calendar);
            l.open_after(tile, card, View::session("started"));
            let t = l.tile(tile).unwrap();
            assert_eq!(t.columns.len(), 2);
            assert_eq!(t.columns[0].key, card, "keep the card's mounted pane");
            assert_eq!(t.columns[0].view, source);
            assert_eq!(t.columns[1].view, View::session("started"));
            assert_eq!(t.back.last().unwrap()[0].view, View::Sessions, "the old list stays in history");
            let started = t.columns[1].key;
            assert_eq!(t.active, started, "keyboard actions target the new session");
            assert!(l.pane_shown(tile, started, true), "a phone navigates to the new session");
            assert!(!l.pane_shown(tile, card, true), "the phone does not stay on the card");
            assert!(l.pane_shown(tile, card, false), "desktop keeps the card alongside");
        }
    }

    #[test]
    fn a_pane_opens_beside_its_session_and_the_bar_still_speaks_for_it() {
        use crate::panes::Pane;
        // A session opened from the list: the tile shows both.
        let mut l = Layout::new(View::Sessions);
        let tile = l.tiles[0].key;
        let key = l.key();
        l.tiles[0].columns.push(Column { key, view: View::session("s1") });
        l.tiles[0].settle();
        assert_eq!(l.bar_column(tile), Some(key));
        // Its Changes: beside the session, which stays; the list goes back
        // in the tile's history.
        l.open_after(tile, key, View::SessionChanges { id: "s1".into() });
        let t = l.tile(tile).unwrap();
        assert_eq!(t.columns.len(), 2);
        assert_eq!(t.columns[0].key, key, "the session's pane is kept, not drawn again");
        assert_eq!(t.columns[1].view, View::SessionChanges { id: "s1".into() });
        assert_eq!(t.back.last().unwrap()[0].view, View::Sessions);
        assert_eq!(l.bar_column(tile), Some(key), "the bar speaks for the session, not its pane");
        let changes = t.columns[1].key;
        assert_eq!(l.find_pane(Pane::Changes, "s1"), Some((tile, changes)));
        assert_eq!(l.find_pane(Pane::Terminal, "s1"), None);
        assert_eq!(l.find_pane(Pane::Changes, "s2"), None);
        // Back to the session on a phone: the pane is there but not shown,
        // and comes forward again.
        l.tiles[0].cursor = 0;
        assert!(l.pane_shown(tile, changes, false), "side by side, both show");
        assert!(!l.pane_shown(tile, changes, true), "a phone shows the session");
        l.reveal(tile, changes);
        assert!(l.pane_shown(tile, changes, true));
        // Another of its panes takes that one's place.
        l.open_after(tile, key, View::SessionTerminal { id: "s1".into() });
        assert_eq!(l.tile(tile).unwrap().columns[0].key, key);
        assert_eq!(l.find_pane(Pane::Changes, "s1"), None);
        // Something else beside it (a doc): the bar speaks for that.
        l.open_after(tile, key, View::Docs { id: "d1".into(), target: None });
        assert_ne!(l.bar_column(tile), Some(key));
        // Another session's pane beside it is not its own.
        l.open_after(tile, key, View::SessionFiles { id: "s2".into() });
        assert_ne!(l.bar_column(tile), Some(key));
    }

    fn r(left: f64, top: f64, width: f64, height: f64) -> Rect {
        Rect { left, top, width, height }
    }

    /// Tiles placed as they open, as (column, when used) for `arrange`.
    #[test]
    fn focus_mode_is_remembered_with_the_layout() {
        let mut l = opened(3);
        l.full = true;
        let back: Layout = serde_json::from_str(&serde_json::to_string(&l).unwrap()).unwrap();
        assert!(back.full);
        assert_eq!(back.tiles, l.tiles, "the tiles keep their places");
        // A layout saved before focus mode loads out of it.
        let mut v = serde_json::to_value(&l).unwrap();
        v.as_object_mut().unwrap().remove("full");
        assert!(!serde_json::from_value::<Layout>(v).unwrap().full);
    }

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

    fn widths(l: &Layout) -> Vec<Option<f64>> {
        let r = l.rects();
        l.tiles.iter().map(|t| r.get(&t.key).map(|x| x.width)).collect()
    }

    #[test]
    fn a_minimized_tile_gives_up_its_room_and_comes_back_to_its_place() {
        let mut l = opened(3);
        l.minimize(1);
        assert_eq!(widths(&l), vec![Some(0.5), None, Some(0.5)], "its column takes no room");
        assert_eq!(stacks(&l), [0, 1, 2], "it keeps its column");
        l.show(1);
        assert_eq!(widths(&l), vec![Some(1.0 / 3.0); 3]);
        assert_eq!(l.rects()[&l.tiles[1].key].left, 1.0 / 3.0, "back in the middle");
        assert_eq!(l.focus, 1);
    }

    #[test]
    fn a_minimized_tile_returns_to_its_place_in_a_stack() {
        let mut l = opened(5); // four columns; the fifth tile stacks under the first
        assert_eq!(stacks(&l), [0, 1, 2, 3, 0]);
        l.minimize(4);
        assert_eq!(l.rects()[&l.tiles[0].key].height, 1.0, "the other takes the column");
        l.show(4);
        let r = l.rects();
        assert_eq!((r[&l.tiles[0].key].top, r[&l.tiles[4].key].top), (0.0, 0.5), "below it again");
    }

    #[test]
    fn the_last_tile_shown_stays() {
        let mut l = opened(2);
        l.minimize(0);
        l.minimize(1);
        assert!(!l.tiles[1].minimized);
        assert_eq!(l.focus, 1);
    }

    #[test]
    fn minimizing_the_focused_tile_moves_the_focus_and_ends_focus_mode() {
        let mut l = opened(3);
        l.touch(0);
        l.focus = 2;
        l.full = true;
        l.minimize(2);
        assert!(!l.full);
        assert_eq!(l.focus, 0, "to the tile used last");
        l.full = true;
        l.focus = 1;
        l.minimize(0);
        assert!(l.full, "another tile's minimizing leaves focus mode on");
    }

    #[test]
    fn closing_keeps_a_tile_shown_and_the_focus_where_it_was() {
        let mut l = opened(3);
        l.minimize(0);
        l.focus = 2;
        l.remove_tile(1);
        assert_eq!(l.focus, 1, "the same tile, now second");
        l.remove_tile(1);
        assert_eq!(l.tiles.len(), 1);
        assert!(!l.tiles[0].minimized, "the minimized one comes back rather than none showing");
    }

    #[test]
    fn a_new_tile_takes_a_column_only_minimized_tiles_hold() {
        let mut l = opened(4);
        l.minimize(3);
        l.open_tile(View::home());
        assert_eq!(stacks(&l), [0, 1, 2, 3, 3]);
        assert_eq!(widths(&l)[4], Some(0.25));
        l.show(3);
        assert_eq!(l.rects()[&l.tiles[3].key].height, 0.5, "they share it");
    }

    #[test]
    fn tiles_are_numbered_in_reading_order() {
        let mut l = opened(3);
        // Moved: the third tile under the first, the first column on the right.
        for (t, c) in l.tiles.iter_mut().zip([1, 0, 1]) {
            t.stack = Some(c);
        }
        assert_eq!(l.reading_order(), [1, 0, 2], "left column first, then top to bottom");
        l.minimize(0);
        assert_eq!(l.reading_order(), [1, 0, 2], "a minimized tile keeps its number");
    }

    #[test]
    fn a_layout_saved_before_minimizing_loads_with_every_tile_shown() {
        let l = opened(2);
        let mut v = serde_json::to_value(&l).unwrap();
        for t in v["tiles"].as_array_mut().unwrap() {
            t.as_object_mut().unwrap().remove("minimized");
        }
        assert!(serde_json::from_value::<Layout>(v).unwrap().tiles.iter().all(|t| !t.minimized));
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

    #[test]
    fn full_columns_reject_only_new_columns_not_more_tiles() {
        let mut l = opened(16);
        let (from, target) = (l.tiles[7].key, l.tiles[0].key);
        let before = l.clone();
        for to in [Drop::at(target, 0.1, 0.25), Drop::at(target, 0.9, 0.75)] {
            assert_eq!(l.check_drop(from, to), Err("Four columns are already open; drop above or below a tile"));
            l.move_tile(from, to);
            assert_eq!(l, before, "no room for a fifth column; it must not be highlighted");
        }
        for to in [Drop::at(target, 0.5, 0.25), Drop::at(target, 0.5, 0.75)] {
            let mut l = before.clone();
            assert_eq!(l.check_drop(from, to), Ok(()), "there is no per-column tile limit");
            l.move_tile(from, to);
            assert_eq!(l.tile(from).unwrap().stack, l.tile(target).unwrap().stack);
            assert_eq!(l.tiles.iter().filter(|t| t.stack == Some(0)).count(), 5);
            assert_eq!(l.tiles[l.focus].key, before.tiles[before.focus].key);
        }
    }

    #[test]
    fn drops_that_are_already_in_place_are_not_advertised() {
        let mut l = opened(9);
        let (first, second, third) = (l.tiles[0].key, l.tiles[4].key, l.tiles[8].key);
        l.minimize(4);
        l.minimize(5);
        l.minimize(6);
        for (from, to) in [
            (first, Drop::Above(first)),
            (first, Drop::Below(first)),
            (first, Drop::LeftOf(first)),
            (first, Drop::RightOf(first)),
            (first, Drop::Above(third)),
            (third, Drop::Below(first)),
            (l.tiles[1].key, Drop::LeftOf(l.tiles[2].key)),
            (l.tiles[2].key, Drop::RightOf(l.tiles[1].key)),
        ] {
            let before = l.clone();
            assert_eq!(l.check_drop(from, to), Err("Already here"));
            l.move_tile(from, to);
            assert_eq!(l, before);
        }
        assert!(l.tile(second).unwrap().minimized);
        assert_eq!(l.check_drop(first, Drop::Below(third)), Ok(()), "reordering within one's column still works");
        l.move_tile(first, Drop::Below(third));
        let column: Vec<_> = l.tiles.iter().filter(|t| !t.minimized && t.stack == Some(0)).map(|t| t.key).collect();
        assert_eq!(column, [third, first]);
    }

    #[test]
    fn drop_keys_survive_closed_or_minimized_tiles_but_not_a_missing_endpoint() {
        let mut l = opened(12);
        let (from, target) = (l.tiles[7].key, l.tiles[8].key);
        l.remove_tile(0);
        l.minimize(0);
        assert_eq!(l.check_drop(from, Drop::Above(target)), Ok(()));
        l.move_tile(from, Drop::Above(target));
        let i = l.tiles.iter().position(|t| t.key == from).unwrap();
        assert_eq!(l.tiles[i + 1].key, target, "keys, not pre-drag indices, locate the tiles");
        for source in [true, false] {
            for close in [true, false] {
                let mut l = l.clone();
                let key = if source { from } else { target };
                let i = l.tiles.iter().position(|t| t.key == key).unwrap();
                if close {
                    l.remove_tile(i);
                } else {
                    l.minimize(i);
                }
                let before = l.clone();
                assert!(l.check_drop(from, Drop::Below(target)).is_err());
                l.move_tile(from, Drop::Below(target));
                assert_eq!(l, before, "a stale drag must not move hidden tiles or unrelated neighbours");
            }
        }
    }

    #[test]
    fn every_allowed_drop_changes_the_visible_layout_and_every_rejection_keeps_it() {
        let visible = |l: &Layout| {
            // Canonicalize column numbers: a column held only by minimized
            // tiles takes no room, even though it keeps its saved number.
            let mut columns: Vec<_> = l.tiles.iter().filter(|t| !t.minimized).filter_map(|t| t.stack).collect();
            columns.sort_unstable();
            columns.dedup();
            l.reading_order()
                .into_iter()
                .filter(|&i| !l.tiles[i].minimized)
                .map(|i| (l.tiles[i].key, columns.iter().position(|c| Some(*c) == l.tiles[i].stack).unwrap()))
                .collect::<Vec<_>>()
        };
        for n in 2..=16 {
            for hidden in [false, true] {
                let mut l = opened(n);
                if hidden {
                    l.minimize(0);
                }
                let before = visible(&l);
                for from in &l.tiles {
                    for target in &l.tiles {
                        for to in [Drop::Above(target.key), Drop::Below(target.key), Drop::LeftOf(target.key), Drop::RightOf(target.key)] {
                            let mut moved = l.clone();
                            moved.move_tile(from.key, to);
                            if l.check_drop(from.key, to).is_ok() {
                                let after = visible(&moved);
                                assert_ne!(after, before, "{n} tiles, hidden {hidden}, {} to {to:?}", from.key);
                                let i = after.iter().position(|t| t.0 == from.key).unwrap();
                                let j = after.iter().position(|t| t.0 == target.key).unwrap();
                                match to {
                                    Drop::Above(_) => {
                                        assert_eq!(i + 1, j);
                                        assert_eq!(after[i].1, after[j].1);
                                    }
                                    Drop::Below(_) => {
                                        assert_eq!(i, j + 1);
                                        assert_eq!(after[i].1, after[j].1);
                                    }
                                    Drop::LeftOf(_) | Drop::RightOf(_) => {
                                        let columns = before.iter().map(|t| t.1).collect::<HashSet<_>>().len();
                                        let alone = l.tiles.iter().filter(|t| !t.minimized && t.stack == from.stack).count() == 1;
                                        assert_eq!(after.iter().map(|t| t.1).collect::<HashSet<_>>().len(), columns + usize::from(!alone));
                                        assert_eq!(after.iter().filter(|t| t.1 == after[i].1).count(), 1, "a new column, not a stack");
                                        if matches!(to, Drop::LeftOf(_)) {
                                            assert_eq!(after[i].1 + 1, after[j].1);
                                        } else {
                                            assert_eq!(after[i].1, after[j].1 + 1);
                                        }
                                    }
                                }
                                assert_eq!(moved.tiles[moved.focus].key, l.tiles[l.focus].key);
                            } else {
                                assert_eq!(moved, l);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_free_column_or_a_singleton_can_still_move_sideways() {
        let mut l = opened(5);
        let (from, target) = (l.tiles[4].key, l.tiles[2].key);
        l.minimize(3);
        assert_eq!(l.check_drop(from, Drop::RightOf(target)), Ok(()), "a minimized-only column is free");
        l.move_tile(from, Drop::RightOf(target));
        assert!(l.tile(from).unwrap().stack > l.tile(target).unwrap().stack);

        let mut l = opened(5);
        let (from, target) = (l.tiles[3].key, l.tiles[0].key);
        assert_eq!(l.check_drop(from, Drop::LeftOf(target)), Ok(()), "a singleton moves its column, not adds one");
        l.move_tile(from, Drop::LeftOf(target));
        assert_eq!(l.tile(from).unwrap().stack, Some(0));
        assert_eq!(l.tile(target).unwrap().stack, Some(1));
    }

    #[test]
    fn a_new_column_reuses_a_minimized_only_slot_without_merging_visible_columns() {
        let mut l = opened(9);
        let keys: Vec<_> = l.tiles.iter().map(|t| t.key).collect();
        for i in [0, 4, 8] {
            l.minimize(i);
        }
        // Three columns are shown; column zero is held only by minimized
        // tiles. Keeping that spare as well as the new column used to make
        // five saved columns: place() capped the last two to column three,
        // silently stacking the dropped tile with the rightmost column.
        let to = Drop::RightOf(keys[2]);
        assert_eq!(l.check_drop(keys[5], to), Ok(()));
        l.move_tile(keys[5], to);
        let shown: HashSet<_> = l.tiles.iter().filter(|t| !t.minimized).map(|t| t.stack).collect();
        assert_eq!(shown.len(), COLUMNS, "the edge promised a new column");
        assert_eq!(l.tiles.iter().filter(|t| !t.minimized && t.stack == l.tile(keys[5]).unwrap().stack).count(), 1);
        assert!(l.tile(keys[2]).unwrap().stack < l.tile(keys[5]).unwrap().stack);
        assert!(l.tile(keys[5]).unwrap().stack < l.tile(keys[3]).unwrap().stack);
        assert_eq!(l.tiles[l.focus].key, keys[7], "focus stays on the last shown focused tile");
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
    /// A session with one of its own panes beside it: they share the tile
    /// evenly (the transcript isn't a list).
    even: bool,
}

/// The identity of a pane's mounted content, separate from its address.
/// Documents observe targets in place; sessions consume targets at mount.
/// A changed key must unmount its old owner/DOM, not rebuild against a
/// disposed owner's reactive attribute state.
fn content_key(mut view: View, visit: u64) -> (View, u64) {
    match &mut view {
        View::Docs { target, .. } => *target = None,
        View::Session { target: Some(_), .. } => return (view, visit),
        _ => {}
    }
    (view, 0)
}

#[component]
fn ColumnView(tile: u64, col: u64) -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    let loading = expect_context::<TileLoading>();
    let mounted = Memo::new(move |was: Option<&bool>| {
        initial_mount(
            was.copied().unwrap_or(false),
            ws.layout.with(|l| {
                let one_pane = loading.one_pane.get();
                l.pane_shown(tile, col, one_pane) && (!one_pane || l.tiles.get(l.focus).is_some_and(|t| t.key == tile))
            }),
            ws.layout.with(|l| l.tiles.get(l.focus).is_some_and(|t| t.key == tile)),
            loading.released.get(),
        )
    });
    let pos = Memo::new(move |_| {
        ws.layout.with(|l| {
            let ti = l.tiles.iter().position(|t| t.key == tile)?;
            let t = &l.tiles[ti];
            let index = t.columns.iter().position(|c| c.key == col)?;
            let even = l.bar_column(tile) == Some(t.columns[0].key) && t.columns.len() == 2;
            Some(Pos { index, cursor: t.cursor, len: t.columns.len(), active: t.active == col && l.focus == ti, even })
        })
    });
    let view =
        Memo::new(move |_| ws.layout.with(|l| l.tile(tile).and_then(|t| t.columns.iter().find(|c| c.key == col)).map(|c| c.view.clone())));
    // A block jump is not a new document. Keep DocPage (and unsaved block
    // and comment drafts) mounted; it observes the target separately.
    let navigation = ws.navigation_for(col);
    let content = Memo::new(move |_| view.get().map(|v| content_key(v, navigation.get())));
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
        // The list beside an item is narrower. A narrow tile reuses the
        // phone's cursor/back navigation instead of squeezing two panes.
        <div
            id=ctx.root_id()
            data-pane-motion=col
            class=move || match pos.get() {
                Some(p) if p.len == 2 && p.index == 0 => format!(
                    "min-w-0 flex-col border-r border-zinc-200 dark:border-zinc-800 md:@min-[40rem]/tile:flex md:@min-[40rem]/tile:grow-0 md:@min-[40rem]/tile:shrink-0 {} {}",
                    if p.even { "md:@min-[40rem]/tile:basis-1/2" } else { "md:@min-[40rem]/tile:basis-[40%]" },
                    if p.cursor == 0 { "flex flex-1" } else { "hidden" },
                ),
                Some(p) => format!("min-w-0 flex-1 flex-col md:@min-[40rem]/tile:flex {}", if p.cursor == p.index { "flex" } else { "hidden" }),
                None => "hidden".to_owned(),
            }
            on:pointerdown=move |_| ws.set_active(tile, col)
        >
            // A pane has its own header only beside another; alone, the tile's bar names it.
            // Keep it mounted when only focus/active changes on pointerdown,
            // or its controls disappear before their click can arrive.
            <Show when=move || pos.get().is_some_and(|p| p.len == 2)>
                <ColumnHeader label pos=Signal::derive(move || pos.get()) view=Signal::derive(move || view.get()) />
            </Show>
            <div class="@container/view relative flex min-h-0 min-w-0 flex-1 flex-col">
                <Show when=move || mounted.get()>
                    <For each=move || content.get() key=|(view, visit)| (view.clone(), *visit) let:content>
                        <InitialView view=content.0 col />
                    </For>
                </Show>
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
    // 24px round a 16px glyph (the bar is 28px), 40px on a touch screen.
    let btn = "flex size-6 shrink-0 items-center justify-center rounded text-muted outline-none focus-visible:ring-2 focus-visible:ring-sky-500/60 pointer-fine:hover:bg-zinc-200 pointer-fine:hover:text-zinc-900 pointer-coarse:size-10 dark:pointer-fine:hover:bg-zinc-800 dark:pointer-fine:hover:text-zinc-100";
    view! {
        <div class=move || format!(
            "flex h-7 shrink-0 items-center gap-1 border-b border-zinc-100 px-2 text-[11px] pointer-coarse:h-11 pointer-coarse:gap-0 dark:border-zinc-900 {}",
            if ctx.active.get() { "text-zinc-700 dark:text-zinc-300" } else { "text-muted" },
        )>
            <Show when=move || pos.get().is_some_and(|p| p.index == 1)>
                <button class=format!("{btn} md:@min-[40rem]/tile:hidden") title="Back to the list" aria-label="Back to the list" on:click=move |_| ws.show_list(ctx.tile)>{crate::icons::chevron_left()}</button>
            </Show>
            <span class="min-w-0 flex-1 truncate">{label}</span>
            {move || session().map(|id| view! {
                <button class=btn title=move || if pinned() { "Unpin" } else { "Pin to the dock" } aria-label=move || if pinned() { "Unpin" } else { "Pin" }
                    on:click=move |_| ws.toggle_pin(&id)>
                    {move || crate::icons::star(pinned())}
                </button>
            })}
            <Show when=move || pos.get().is_some_and(|p| p.len == 2)>
                <button class=btn title="Close this pane" aria-label="Close this pane" on:click=move |_| ctx.close()>{crate::icons::x()}</button>
            </Show>
        </div>
    }
}

fn render_view(view: View) -> AnyView {
    match view {
        View::Session { id, target } => view! { <crate::session::SessionView id target /> }.into_any(),
        View::SessionOutputs { id, output } => view! { <crate::panes::SessionPane id pane=crate::panes::Pane::Outputs output /> }.into_any(),
        View::SessionChanges { id } => view! { <crate::panes::SessionPane id pane=crate::panes::Pane::Changes /> }.into_any(),
        View::SessionTerminal { id } => view! { <crate::panes::SessionPane id pane=crate::panes::Pane::Terminal /> }.into_any(),
        View::SessionFiles { id } => view! { <crate::panes::SessionPane id pane=crate::panes::Pane::Files /> }.into_any(),
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
                    View::Chats => view! { <crate::chats::ChatsPage /> }.into_any(),
                    View::Analytics => view! { <crate::analytics::AnalyticsPage /> }.into_any(),
                    View::Activity => view! { <crate::monitor::ActivityPage /> }.into_any(),
                    View::Memex => view! { <crate::memex::MemexPage /> }.into_any(),
                    // Each realm has its own mail, calendars and contacts:
                    // drawn again for the realm you pick.
                    View::Mail => (move || { crate::api::realm().track(); view! { <crate::mail::MailPage /> } }).into_any(),
                    View::Chat => view! { <crate::chat::ChatPage /> }.into_any(),
                    View::Conversation { target } => (move || { crate::api::realm().track(); view! { <crate::brief::ConversationPage target=target.clone() /> } }).into_any(),
                    View::Brief => (move || { crate::api::realm().track(); view! { <crate::brief::BriefPage /> } }).into_any(),
                    View::Calendar => (move || { crate::api::realm().track(); view! { <crate::calendar::CalendarPage /> } }).into_any(),
                    View::Contacts => (move || { crate::api::realm().track(); view! { <crate::contacts::ContactsPage /> } }).into_any(),
                    View::Photos => view! { <crate::photos::PhotosPage /> }.into_any(),
                    View::Decisions => view! { <crate::decisions::DecisionsPage /> }.into_any(),
                    View::Work { id } if id.is_empty() => view! { <crate::work::WorkPage /> }.into_any(),
                    View::Work { id } => view! { <crate::work::WorkItemPage id=id.clone() /> }.into_any(),
                    View::Projects { id } if id.is_empty() => view! { <crate::projects::ProjectsPage /> }.into_any(),
                    View::Projects { id } => view! { <crate::projects::ProjectPage id=id.clone() /> }.into_any(),
                    View::Docs { id, .. } if id.is_empty() => view! { <crate::docs::DocsPage /> }.into_any(),
                    View::Docs { id, .. } => view! { <crate::docs::DocPage id=id.clone() /> }.into_any(),
                    View::Collections { name } => view! { <crate::collections::CollectionsPage name=name.clone() /> }.into_any(),
                    View::App { name, record } => view! { <crate::views::ViewPage name=name.clone() record=record.clone() /> }.into_any(),
                    View::Realms => view! { <crate::explorer::RealmsMap /> }.into_any(),
                    View::Explore { realm, section, kind, id } => view! { <crate::explorer::ExplorePage realm=realm.clone() section=section.clone() kind=kind.clone() id=id.clone() /> }.into_any(),
                    View::Session { .. }
                    | View::SessionChanges { .. }
                    | View::SessionTerminal { .. }
                    | View::SessionFiles { .. }
                    | View::SessionOutputs { .. } => ().into_any(),
                }}
            </ScrollBox>
        }
        .into_any(),
    }
}

/// A path segment, percent-encoded.
pub fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&b) {
            out.push(char::from(b));
        } else {
            use std::fmt::Write;
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn decode(s: &str) -> String {
    decode_fragment(&s.replace('+', " "))
}

/// Fragments, unlike query parameters, preserve a literal plus sign.
fn decode_fragment(s: &str) -> String {
    let mut out = Vec::with_capacity(s.len());
    let mut bytes = s.bytes();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let Some((hi, lo)) =
                bytes.next().and_then(|b| char::from(b).to_digit(16)).zip(bytes.next().and_then(|b| char::from(b).to_digit(16)))
            else {
                return s.to_owned();
            };
            out.push((hi * 16 + lo) as u8);
        } else {
            out.push(b);
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}
