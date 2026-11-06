//! A session's panes: its Changes, Terminal, Files and Outputs, each a view
//! of its own beside the session (the transcript is never swapped out), or
//! in a tile of its own when the tile has no room for both.

use c6_api::{EventKind, SessionStatus};
use leptos::prelude::*;

use crate::api;
use crate::util::ErrorBox;
use crate::workspace::{ColumnCtx, How, ScrollBox, View, Workspace};

/// How wide a tile must be for a pane beside the transcript.
const BESIDE_MIN: i32 = 1100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pane {
    Changes,
    Terminal,
    Files,
    Outputs,
}

impl Pane {
    pub fn name(self) -> &'static str {
        match self {
            Pane::Changes => "Changes",
            Pane::Terminal => "Terminal",
            Pane::Files => "Files",
            Pane::Outputs => "Outputs",
        }
    }

    pub fn view(self, id: String) -> View {
        match self {
            Pane::Changes => View::SessionChanges { id },
            Pane::Terminal => View::SessionTerminal { id },
            Pane::Files => View::SessionFiles { id },
            Pane::Outputs => View::SessionOutputs { id, output: String::new() },
        }
    }
}

/// Opens `view` from a session's column so the transcript stays in sight:
/// beside it when its tile is wide enough for both (and on a phone, where
/// it then fills the screen, ‹ going back), else in a tile of its own;
/// `new_tile` (⌘/Ctrl-click): a new tile.
pub fn open(col: ColumnCtx, view: View, new_tile: bool) {
    if new_tile || !roomy(&col) {
        col.open(view, How::NewTile);
    } else {
        col.ws.open_after(col.tile, col.col, view);
    }
}

/// Where a session's pane is, and whether it's on screen now (tracked: its
/// button shows it pressed while it is).
pub fn shown(ws: Workspace, pane: Pane, id: &str) -> Option<(u64, u64, bool)> {
    let one_pane = crate::util::phone();
    ws.layout.with(|l| l.find_pane(pane, id).map(|(t, c)| (t, c, l.pane_shown(t, c, one_pane))))
}

/// A session's pane: closed if it's on screen, brought forward if it's
/// open but out of sight (a phone went back to the transcript, its tile
/// minimized), else opened.
pub fn toggle(col: ColumnCtx, pane: Pane, id: &str, new_tile: bool) {
    match untrack(|| shown(col.ws, pane, id)) {
        Some((tile, c, true)) if !new_tile => col.ws.close_column(tile, c),
        Some((tile, c, false)) if !new_tile => col.ws.reveal(tile, c),
        _ => open(col, pane.view(id.to_owned()), new_tile),
    }
}

/// A session's pane, opened (or brought forward) unless it's on screen.
pub fn show(col: ColumnCtx, pane: Pane, id: &str, new_tile: bool) {
    match untrack(|| shown(col.ws, pane, id)) {
        Some((_, _, true)) if !new_tile => {}
        Some((tile, c, false)) if !new_tile => col.ws.reveal(tile, c),
        _ => open(col, pane.view(id.to_owned()), new_tile),
    }
}

/// Its Outputs pane showing one output: the open pane turns to it (and
/// comes forward), or one opens.
pub fn show_output(col: ColumnCtx, id: &str, output: String) {
    let view = View::SessionOutputs { id: id.to_owned(), output };
    match col.ws.layout.with_untracked(|l| l.find_pane(Pane::Outputs, id)) {
        Some((tile, c)) => {
            col.ws.replace(tile, c, view);
            col.ws.reveal(tile, c);
        }
        None => open(col, view, false),
    }
}

/// Whether the column's tile has room for a pane beside the transcript.
fn roomy(col: &ColumnCtx) -> bool {
    crate::util::phone()
        || document()
            .get_element_by_id(&col.root_id())
            .and_then(|el| el.closest("section").ok().flatten())
            .is_some_and(|tile| tile.client_width() >= BESIDE_MIN)
}

/// A session's pane as a view: the session's Changes, Terminal or Files in
/// the folder it works in (its worktree, if it has one), or its Outputs
/// (`output`: the one shown; empty for the newest).
#[component]
pub fn SessionPane(id: String, pane: Pane, #[prop(optional)] output: String) -> impl IntoView {
    if pane == Pane::Outputs {
        let feed = crate::session::Feed::open(id.clone(), crate::session::Window::Tail);
        let chosen = RwSignal::new(Some(output).filter(|o| !o.is_empty()));
        return view! {
            <ScrollBox>
                <div class="px-4 py-3"><crate::outputs::Outputs session=id events=feed.events chosen /></div>
            </ScrollBox>
        }
        .into_any();
    }
    let live = api::live();
    let data = crate::resources::session(id.clone());
    let detail = data.detail;
    // Where it works: its first event says. Asked again once it has started.
    let started = {
        let id = id.clone();
        Memo::new(move |_| live.sessions.with(|ss| ss.iter().find(|s| s.id == id).is_some_and(|s| s.status != SessionStatus::Starting)))
    };
    let first = LocalResource::new(move || {
        started.track();
        let (folder, id) = (data.folder.get(), id.clone());
        async move {
            if folder.is_none() {
                let evs = api::session_events_before(id, 2, 1, None).await.ok()?;
                evs.into_iter().find_map(|e| match e.kind {
                    EventKind::Started { cwd, .. } => Some(cwd),
                    _ => None,
                })
            } else {
                folder
            }
        }
    });
    Effect::new(move || {
        if let Some(folder) = first.get().flatten()
            && data.folder.get_untracked().as_ref() != Some(&folder)
        {
            data.folder.set(Some(folder));
        }
    });
    let session = Signal::derive(move || detail.get().map(|r| r.map(|d| d.session)));
    let folder = Signal::derive(move || data.folder.get().or_else(|| first.get().flatten()));
    view! { <PaneContent pane session folder /> }.into_any()
}

/// Native pane views consume their parent's confirmed session and actual
/// folder. They never fetch session metadata before asking the worker.
#[component]
fn PaneContent(pane: Pane, session: Signal<Option<Result<c6_api::Session, String>>>, folder: Signal<Option<String>>) -> impl IntoView {
    let note = |text: &'static str| view! { <p class="py-6 text-center text-sm text-muted">{text}</p> }.into_any();
    view! {
        <ScrollBox>
            <div class="px-4 py-3">
                <Suspense fallback=|| ()>
                    {move || {
                        let s = match session.get()? {
                            Ok(s) => s,
                            Err(e) => return Some(view! { <ErrorBox message=e /> }.into_any()),
                        };
                        let cwd = folder.get();
                        let w = s.worker_id.to_string();
                        let ssh = s.ssh;
                        Some(match (pane, cwd) {
                            (_, None) => note("Available once the session has started."),
                            // The worker can't look at that machine's files.
                            (Pane::Changes | Pane::Files, Some(_)) if ssh.is_some() => note("Not for a session working over SSH."),
                            (Pane::Changes, Some(c)) => view! { <crate::files::Changes worker_id=w path=c /> }.into_any(),
                            (Pane::Files, Some(c)) => view! { <crate::files::FileBrowser worker_id=w root=c /> }.into_any(),
                            (_, Some(c)) => view! { <crate::term::Terminals worker_id=w default_cwd=c ssh /> }.into_any(),
                        })
                    }}
                </Suspense>
            </div>
        </ScrollBox>
    }
    .into_any()
}
