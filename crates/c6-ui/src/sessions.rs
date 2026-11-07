//! Sessions list and the new-session form.

use c6_api::work::WorkRef;
use c6_api::{Connection, CreateSession, Session, SessionStatus, WorkerStatus};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::icons;
use crate::icons::{Icon, Size, icon};
use crate::session::{ModelPicker, SEND, SEND_EMPTY, SEND_READY, SEND_SHAPE, send_icon};
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, GROWS, INPUT, StatusPill, ago, clock, escapable, storage_get, storage_set};
use crate::workspace::{Go, How, Popover, View, below, use_column, use_workspace};

/// Placeholders in the text's own font, in readable grey (a field's value
/// may be monospace).
const PLACEHOLDER: &str = "placeholder:font-sans placeholder:text-muted";

/// A choice among a few, as a chip to tap: filled when chosen.
fn chip(on: bool) -> &'static str {
    if on {
        "rounded-full border border-sky-600 bg-sky-600 px-3 py-1 text-xs text-white pointer-coarse:py-1.5 pointer-coarse:text-sm dark:border-sky-500"
    } else {
        "rounded-full border border-zinc-300 px-3 py-1 text-xs text-soft hover:bg-zinc-100 pointer-coarse:py-1.5 pointer-coarse:text-sm dark:border-zinc-700 dark:hover:bg-zinc-800"
    }
}

/// How the list is ordered.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Sort {
    /// The sessions you viewed, newest first, then the rest by last activity.
    #[default]
    Viewed,
    Activity,
    Title,
}

impl Sort {
    const ALL: [Sort; 3] = [Sort::Viewed, Sort::Activity, Sort::Title];

    fn label(self) -> &'static str {
        match self {
            Sort::Viewed => "Recently viewed",
            Sort::Activity => "Last activity",
            Sort::Title => "Title",
        }
    }
}

/// Show's choices (value, label); "" is All.
const SHOWS: [(&str, &str); 4] = [("", "All"), ("running", "Running"), ("needs", "Needs you"), ("idle", "Idle")];

/// Whether Show's choice takes a session: by the state its row says.
fn shows(s: &Session, show: &str) -> bool {
    match show {
        "" => true,
        "needs" => s.awaiting_approval,
        word => recent_state(s).1 == word,
    }
}

/// The sessions in the order `sort` says: for Recently viewed, those you
/// viewed (`viewed`: id and when) newest first, then the rest by last
/// activity.
fn sorted(mut list: Vec<Session>, sort: Sort, viewed: &[(String, i64)]) -> Vec<Session> {
    use std::cmp::Reverse;
    match sort {
        Sort::Viewed => {
            let at = |s: &Session| viewed.iter().find(|(id, _)| *id == s.id).map(|(_, at)| *at);
            list.sort_by_key(|s| match at(s) {
                Some(t) => (false, Reverse(t)),
                None => (true, Reverse(s.updated_at)),
            });
        }
        Sort::Activity => list.sort_by_key(|s| Reverse(s.updated_at)),
        Sort::Title => list.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()).then(b.updated_at.cmp(&a.updated_at))),
    }
    list
}

/// Labels the head puts among a session's own: a template run's.
const HEAD_SET: [&str; 3] = ["template:", "run:", "started-by:"];

/// The labels you gave a session: not the automatic ones (`repo:`,
/// `host:`, `worker:`, `folder:`, `model:`), nor a template run's.
fn own_labels(s: &Session) -> Vec<String> {
    s.labels.iter().filter(|l| c6_api::labels::check_user_label(l).is_ok() && !HEAD_SET.iter().any(|k| l.starts_with(k))).cloned().collect()
}

/// Whether any of the list's choices isn't the default (All, no labels,
/// Recently viewed, no groups): the phone's sliders button shows a dot.
fn non_default(show: &str, labels: &[String], sort: Sort, levels: &[String]) -> bool {
    !show.is_empty() || !labels.is_empty() || sort != Sort::default() || !levels.is_empty()
}

/// A folder's last part ("carbon6" for ~/projects/carbon6).
fn last_part(path: &str) -> String {
    let p = path.trim_end_matches('/');
    p.rsplit('/').next().filter(|x| !x.is_empty()).unwrap_or(path).to_owned()
}

/// A folder in its `~/` form when it is under a home folder.
fn home_form(path: &str) -> String {
    for root in ["/home/", "/Users/"] {
        if let Some(rest) = path.strip_prefix(root) {
            return match rest.split_once('/') {
                Some((_, after)) if !after.is_empty() => format!("~/{after}"),
                _ => "~".to_owned(),
            };
        }
    }
    match path.strip_prefix("/root") {
        Some("") => "~".to_owned(),
        Some(rest) if rest.starts_with('/') => format!("~{rest}"),
        _ => path.to_owned(),
    }
}

/// Native title text for where and how a new session runs: "on thalassa ·
/// ~/projects/carbon6 · opus · high"; `short` (a phone's chip):
/// "thalassa · carbon6 · opus".
fn summary(worker: &str, ssh: Option<&str>, folder: &str, model: &str, effort: &str, short: bool) -> String {
    let place = match ssh {
        Some(t) if folder.is_empty() => format!("⇄ {t}"),
        Some(t) if short => format!("⇄ {t}:{}", last_part(folder)),
        Some(t) => format!("⇄ {t}:{folder}"),
        None if folder.is_empty() => "scratch folder".to_owned(),
        None if short => last_part(folder),
        None => home_form(folder),
    };
    let mut parts = vec![worker.to_owned(), place];
    parts.extend([model, if short { "" } else { effort }].into_iter().filter(|x| !x.is_empty()).map(str::to_owned));
    let line = parts.join(" · ");
    if short { line } else { format!("on {line}") }
}

/// A visible relationship, without interpreting symbols inside either name.
pub(crate) fn relationship_label(before: String, after: String) -> impl IntoView {
    view! { {before}" "{icon(Icon::ArrowLeftRight, Size::S14)}" "{after} }
}

/// The visible counterpart of `summary`; native titles remain plain text.
fn summary_label(worker: &str, ssh: Option<&str>, folder: &str, model: &str, effort: &str, short: bool) -> AnyView {
    let Some(target) = ssh else { return summary(worker, None, folder, model, effort, short).into_any() };
    let target = if folder.is_empty() {
        target.to_owned()
    } else if short {
        format!("{target}:{}", last_part(folder))
    } else {
        format!("{target}:{folder}")
    };
    let before = if short { worker.to_owned() } else { format!("on {worker}") };
    let rest = [model, if short { "" } else { effort }].into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · ");
    let rest = if rest.is_empty() { rest } else { format!(" · {rest}") };
    view! { {relationship_label(before, target)}{rest} }.into_any()
}

/// The list's choices: Show (and its labels: the shared label filter), Sort
/// and Group (kept in the head, shared with the rail).
#[derive(Clone, Copy)]
struct ListChoices {
    show: RwSignal<String>,
    labels: RwSignal<Vec<String>>,
    sort: RwSignal<Sort>,
    grouping: Grouping,
}

impl ListChoices {
    fn changed(&self) -> bool {
        non_default(&self.show.get(), &self.labels.get(), self.sort.get(), &self.grouping.levels.get())
    }

    /// Back to All, Recently viewed, no groups.
    fn reset(&self) {
        self.show.set(String::new());
        self.labels.set(Vec::new());
        self.sort.set(Sort::default());
        if !self.grouping.levels.with_untracked(Vec::is_empty) {
            self.grouping.choose(Vec::new());
        }
    }

    /// Show ▾'s button: what it shows, if not everything.
    fn show_label(&self) -> String {
        let show = self.show.get();
        let state = SHOWS.iter().find(|(v, _)| *v == show).map(|(_, l)| *l).filter(|_| !show.is_empty());
        let labels = self.labels.get();
        let labels = match labels.as_slice() {
            [] => None,
            [one] => Some(one.clone()),
            many => Some(format!("{} labels", many.len())),
        };
        match (state, labels) {
            (None, None) => "Show: All".to_owned(),
            (Some(s), None) => format!("Show: {s}"),
            (None, Some(l)) => format!("Show: {l}"),
            (Some(s), Some(l)) => format!("Show: {s} · {l}"),
        }
    }
}

#[component]
pub fn SessionsPage() -> impl IntoView {
    let root = NodeRef::<leptos::html::Main>::new();
    let hover = Hover::new();
    let like_this = RwSignal::new(None);
    let live = api::live();
    let now = clock();
    let connections = crate::resources::connections();
    let query = RwSignal::new(String::new());
    let choices = ListChoices {
        show: RwSignal::new(String::new()),
        labels: crate::labels::use_filter().0,
        sort: RwSignal::new(Sort::default()),
        grouping: use_grouping(),
    };
    let folded = Folded::load(Folded::PAGE);
    // The sessions you viewed, for Recently viewed.
    let recent = use_recent(usize::MAX, |_| false);
    let sheet = RwSignal::new(false);
    let search = move |ev: web_sys::Event| query.set(event_target_value(&ev));

    view! {
        <main node_ref=root class="mx-auto max-w-6xl space-y-4 px-3 py-4 @min-[40rem]/view:space-y-5 @min-[40rem]/view:px-4 @min-[40rem]/view:py-6">
            <div class="hidden flex-wrap items-center gap-2 @min-[40rem]/view:flex">
                <h1 class="mr-2 text-lg font-semibold">"Sessions"</h1>
                <input type="search" class=format!("{INPUT} {PLACEHOLDER} min-w-48 max-w-sm flex-1") placeholder="Search titles, labels, workers, folders"
                    prop:value=move || query.get() on:input=search />
                <span class="flex-1"></span>
                <ListMenus choices />
            </div>
            <div class="flex items-center gap-2 @min-[40rem]/view:hidden">
                <label class="relative min-w-0 flex-1">
                    <span class="pointer-events-none absolute left-2.5 top-1/2 flex -translate-y-1/2 text-muted">{icons::search()}</span>
                    <input type="search" class=format!("{INPUT} {PLACEHOLDER} pl-8") placeholder="Search" aria-label="Search sessions"
                        prop:value=move || query.get() on:input=search />
                </label>
                <button class=format!("{} relative", icons::BAR_BUTTON) title="Filter, sort and group" aria-label="Filter, sort and group"
                    aria-haspopup="dialog" aria-expanded=move || sheet.get().to_string() on:click=move |_| sheet.set(true)>
                    {icons::sliders()}
                    {move || choices.changed().then(|| view! {
                        <span class="absolute right-1 top-1 size-2 rounded-full bg-sky-600 dark:bg-sky-400" title="Not the default"></span>
                    })}
                </button>
            </div>
            // Later model-list loads must not detach the new-session form
            // and interrupt an edit in its focused model input.
            <Transition fallback=|| ()>
                {move || connections.get().map(|r| match r {
                    Ok(conns) if conns.is_empty() => view! {
                        <div class="rounded-lg border border-dashed border-zinc-300 p-6 text-center text-sm text-soft dark:border-zinc-700">
                            "Add an LLM connection in " <Go to=View::Settings { note: None, section: "models".into() } class="underline">"Settings"</Go> " to start a session."
                        </div>
                    }.into_any(),
                    Ok(conns) => view! { <SessionForm connections=conns /> }.into_any(),
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                })}
            </Transition>

            <div class="divide-y divide-zinc-200 overflow-hidden rounded-lg border border-zinc-200 dark:divide-zinc-800 dark:border-zinc-800">
                {move || {
                    let q = query.get().to_lowercase();
                    let show = choices.show.get();
                    let wanted = choices.labels.get();
                    let realm = api::realm().get();
                    let viewed: Vec<(String, i64)> = recent.with(|r| r.iter().map(|(s, at)| (s.id.clone(), *at)).collect());
                    let sessions: Vec<Session> = live
                        .sessions
                        .get()
                        .into_iter()
                        .filter(|s| shows(s, &show))
                        .filter(|s| crate::labels::matches(s, &wanted))
                        .filter(|s| realm.is_empty() || s.realm == realm)
                        .filter(|s| {
                            q.is_empty()
                                || s.labels.iter().chain(s.auto_labels.iter()).any(|l| l.to_lowercase().contains(&q))
                                || s.title.to_lowercase().contains(&q)
                                || s.cwd.to_lowercase().contains(&q)
                                || live.session_worker(s).to_lowercase().contains(&q)
                        })
                        .collect();
                    if sessions.is_empty() {
                        let msg = if wanted.is_empty() && q.is_empty() && show.is_empty() { "No sessions yet." } else { "No sessions match." };
                        view! { <p class="px-4 py-8 text-center text-sm text-muted">{msg}</p> }.into_any()
                    } else {
                        let sessions = sorted(sessions, choices.sort.get(), &viewed);
                        grouped(sessions, &choices.grouping.levels.get(), String::new(), folded, &|s| view! { <SessionRow s now hover /> }.into_any(), false)
                    }
                }}
            </div>
        </main>
        <SessionPreview hover like_this />
        {move || like_this.get().map(|seed| view! { <NewSessionDialog seed on_close=move |()| like_this.set(None) /> })}
        <Show when=move || sheet.get()>
            <Sheet root open=sheet title="Filter, sort and group" reset=Callback::new(move |()| choices.reset())>
                <div class="space-y-5">
                    <section class="space-y-2"><ShowChips choices /></section>
                    <section class="space-y-2"><LabelChips choices /></section>
                    <section class="space-y-2"><SortChips choices /></section>
                    <section class="space-y-2"><GroupChips choices /></section>
                </div>
            </Sheet>
        </Show>
    }
}

/// The desktop's Show ▾, Sort ▾ and Group ▾, each opening its choices.
#[component]
fn ListMenus(choices: ListChoices) -> impl IntoView {
    let (show_at, sort_at, group_at) = (RwSignal::new(None), RwSignal::new(None), RwSignal::new(None));
    let button = "inline-flex shrink-0 items-center gap-1 rounded-md border border-zinc-200 px-2.5 py-1.5 text-xs text-soft hover:bg-zinc-100 dark:border-zinc-800 dark:hover:bg-zinc-800";
    view! {
        <button class=button aria-haspopup="dialog" on:click=move |ev| show_at.set(Some(below(&ev, 320.0)))>
            {move || choices.show_label()} <span class="text-muted">{crate::icons::chevron_down()}</span>
        </button>
        <button class=button aria-haspopup="dialog" on:click=move |ev| sort_at.set(Some(below(&ev, 224.0)))>
            {move || format!("Sort: {}", choices.sort.get().label())} <span class="text-muted">{crate::icons::chevron_down()}</span>
        </button>
        <button class=button aria-haspopup="dialog" on:click=move |ev| group_at.set(Some(below(&ev, 320.0)))>
            {move || match choices.grouping.levels.get().as_slice() {
                [] => "Group: none".to_owned(),
                levels => format!("Group: {}", levels.join(" › ")),
            }}
            <span class="text-muted">{crate::icons::chevron_down()}</span>
        </button>
        <Popover at=show_at class="w-80 space-y-3">
            <div class="space-y-2"><ShowChips choices /></div>
            <div class="space-y-2"><LabelChips choices /></div>
        </Popover>
        <Popover at=sort_at class="w-56 space-y-2">
            <SortChips choices done=Callback::new(move |()| sort_at.set(None)) />
        </Popover>
        <Popover at=group_at class="w-80 space-y-2">
            <GroupChips choices />
        </Popover>
    }
}

#[component]
fn ChipHeading(text: &'static str) -> impl IntoView {
    view! { <h3 class="text-[11px] font-semibold uppercase tracking-wide text-muted">{text}</h3> }
}

/// Show: All, Running, Needs you, Idle.
#[component]
fn ShowChips(choices: ListChoices) -> impl IntoView {
    view! {
        <ChipHeading text="Show" />
        <div class="flex flex-wrap gap-1.5" role="radiogroup" aria-label="Show">
            {SHOWS.iter().map(|&(v, l)| {
                let on = move || choices.show.with(|s| s == v);
                view! {
                    <button role="radio" aria-checked=move || on().to_string() class=move || chip(on()) on:click=move |_| choices.show.set(v.to_owned())>{l}</button>
                }
            }).collect_view()}
        </div>
    }
}

/// The realm's labels (yours) to show only sessions with, and a field for
/// any other (the automatic ones too).
#[component]
fn LabelChips(choices: ListChoices) -> impl IntoView {
    let live = api::live();
    let filter = choices.labels;
    let mine = Memo::new(move |_| {
        let realm = api::realm().get();
        let mut all: Vec<String> =
            live.sessions.with(|ss| ss.iter().filter(|s| realm.is_empty() || s.realm == realm).flat_map(own_labels).collect());
        all.extend(filter.get());
        all.sort();
        all.dedup();
        all
    });
    let draft = RwSignal::new(String::new());
    let list_id = format!("c6-show-labels-{}", js_sys::Math::random().to_bits());
    let add = move || {
        let new = c6_api::labels::parse_labels(&draft.get_untracked());
        filter.update(|f| {
            for l in new {
                if !f.contains(&l) {
                    f.push(l);
                }
            }
        });
        draft.set(String::new());
    };
    view! {
        <ChipHeading text="Labels" />
        <div class="flex flex-wrap items-center gap-1.5">
            {move || mine.get().into_iter().map(|l| {
                let on = { let l = l.clone(); Signal::derive(move || filter.with(|f| f.contains(&l))) };
                let flip = l.clone();
                view! {
                    <button aria-pressed=move || on.get().to_string() class=move || chip(on.get())
                        on:click=move |_| filter.update(|f| match f.iter().position(|x| *x == flip) {
                            Some(i) => { f.remove(i); }
                            None => f.push(flip.clone()),
                        })>{l}</button>
                }
            }).collect_view()}
            <input class=format!("{INPUT} {PLACEHOLDER} w-36! py-1 font-mono text-xs") list=list_id.clone() placeholder="Another label…" aria-label="Show only sessions with this label"
                prop:value=move || draft.get() on:input=move |ev| draft.set(event_target_value(&ev))
                on:change=move |_| add()
                on:keydown=move |ev| if ev.key() == "Enter" { ev.prevent_default(); add(); } />
            <datalist id=list_id>
                {move || live.sessions.with(|ss| crate::labels::known_labels(ss)).into_iter().map(|l| view! { <option value=l></option> }).collect_view()}
            </datalist>
        </div>
    }
}

/// Sort: Recently viewed, Last activity, Title; `done` after a choice.
#[component]
fn SortChips(choices: ListChoices, #[prop(optional)] done: Option<Callback<()>>) -> impl IntoView {
    view! {
        <ChipHeading text="Sort" />
        <div class="flex flex-wrap gap-1.5" role="radiogroup" aria-label="Sort">
            {Sort::ALL.into_iter().map(|v| {
                let on = move || choices.sort.get() == v;
                view! {
                    <button role="radio" aria-checked=move || on().to_string() class=move || chip(on())
                        on:click=move |_| { choices.sort.set(v); if let Some(d) = done { d.run(()) } }>{v.label()}</button>
                }
            }).collect_view()}
        </div>
    }
}

/// The label keys to group by: repo, host and model, then those of your
/// labels.
fn label_keys(sessions: &[Session]) -> Vec<String> {
    let mut keys: Vec<String> = ["repo", "host", "model"].map(str::to_owned).to_vec();
    for s in sessions {
        for l in own_labels(s) {
            if let Some((k, _)) = l.split_once(':')
                && !keys.iter().any(|x| x == k)
            {
                keys.push(k.to_owned());
            }
        }
    }
    keys
}

/// `levels` grouped first by `key` (the rest as they were).
fn outermost(levels: &[String], key: &str) -> Vec<String> {
    std::iter::once(key.to_owned()).chain(levels.iter().skip(1).filter(|k| *k != key).cloned()).collect()
}

/// Group: None, Worker, Folder, Label (and by which label key); once
/// grouped, the levels to nest further.
#[component]
fn GroupChips(choices: ListChoices) -> impl IntoView {
    let live = api::live();
    let g = choices.grouping;
    let first = Memo::new(move |_| g.levels.with(|l| l.first().cloned()));
    let by_label = Memo::new(move |_| first.get().is_some_and(|k| k != "worker" && k != "folder"));
    let keys = Memo::new(move |_| live.sessions.with(|ss| label_keys(ss)));
    let by = move |key: &str| g.choose(outermost(&g.levels.get_untracked(), key));
    let plain = [("Worker", "worker"), ("Folder", "folder")];
    view! {
        <ChipHeading text="Group" />
        <div class="flex flex-wrap gap-1.5" role="radiogroup" aria-label="Group">
            <button role="radio" aria-checked=move || first.with(Option::is_none).to_string() class=move || chip(first.with(Option::is_none))
                on:click=move |_| g.choose(Vec::new())>"None"</button>
            {plain.into_iter().map(|(l, k)| {
                let on = move || first.with(|f| f.as_deref() == Some(k));
                view! { <button role="radio" aria-checked=move || on().to_string() class=move || chip(on()) on:click=move |_| by(k)>{l}</button> }
            }).collect_view()}
            <button role="radio" aria-checked=move || by_label.get().to_string() class=move || chip(by_label.get())
                on:click=move |_| if !by_label.get_untracked() {
                    // Your first label key, else the repository.
                    let keys = keys.get_untracked();
                    by(keys.get(3).or(keys.first()).map(String::as_str).unwrap_or("repo"));
                }>"Label"</button>
        </div>
        {move || by_label.get().then(|| view! {
            <div class="flex flex-wrap items-center gap-1.5 pl-2" role="radiogroup" aria-label="Group by which label">
                <span class="text-xs text-muted">"by"</span>
                {keys.get().into_iter().map(|k| {
                    let on = { let k = k.clone(); Signal::derive(move || first.with(|f| f.as_deref() == Some(k.as_str()))) };
                    let pick = k.clone();
                    view! { <button role="radio" aria-checked=move || on.get().to_string() class=move || format!("{} font-mono", chip(on.get())) on:click=move |_| by(&pick)>{k}</button> }
                }).collect_view()}
            </div>
        })}
        {move || first.with(Option::is_some).then(|| view! {
            <div class="border-t border-zinc-200 pt-2 dark:border-zinc-800"><GroupBar grouping=g /></div>
        })}
    }
}

/// A sheet from the owning tile's bottom, including in narrow desktop
/// tiles; without a tile, fall back to the viewport. A tap above it, Done
/// or Esc closes it; `reset` puts Reset at its top right.
#[component]
fn Sheet(
    root: NodeRef<leptos::html::Main>,
    open: RwSignal<bool>,
    #[prop(into)] title: String,
    #[prop(optional)] reset: Option<Callback<()>>,
    children: ChildrenFn,
) -> impl IntoView {
    use wasm_bindgen::JsCast;
    let mount = root.get_untracked().and_then(|root| root.closest("[data-tile-surface]").ok().flatten());
    let position = if mount.is_some() { "absolute" } else { "fixed" };
    // The phone shell already shrinks the tile above the keyboard.
    let keyboard = if mount.is_some() { "" } else { "phone-sheet" };
    let mount = mount.unwrap_or_else(|| document().body().expect("body").unchecked_into());
    let close = move || {
        open.try_set(false);
    };
    let pop = escapable(move || open.get(), close);
    let label = title.clone();
    view! {
        <leptos::portal::Portal mount=mount>
        <div class=format!("c6-session-filter-scrim {position} inset-0 z-40 bg-black/30") on:click=move |_| close()></div>
        <div data-pop=pop.clone() role="dialog" aria-label=label.clone()
            class=format!("{keyboard} {position} inset-x-0 bottom-0 z-50 flex max-h-[85%] flex-col rounded-t-xl border-t border-zinc-200 bg-white text-sm shadow-xl dark:border-zinc-700 dark:bg-zinc-900")>
            <div class="flex shrink-0 items-center gap-2 border-b border-zinc-200 py-1 pl-4 pr-2 dark:border-zinc-800">
                <h2 class="flex-1 py-2 font-medium">{title.clone()}</h2>
                {reset.map(|r| view! { <button class="rounded-md px-3 py-2 text-sky-700 dark:text-sky-400" on:click=move |_| r.run(())>"Reset"</button> })}
            </div>
            <div class="min-h-0 flex-1 overflow-y-auto px-4 py-4">{children()}</div>
            <div class="notice-safe shrink-0 border-t border-zinc-200 px-4 pt-3 dark:border-zinc-800">
                <button class=format!("{BUTTON} w-full py-2") on:click=move |_| close()>"Done"</button>
            </div>
        </div>
        </leptos::portal::Portal>
    }
}

/// The sessions you viewed lately (kept by the head, the same on every
/// device), newest first, as the live feed has them, with when you viewed
/// each: up to `limit`, leaving out those `skip` says.
pub(crate) fn use_recent(limit: usize, skip: impl Fn(&str) -> bool + Send + Sync + 'static) -> Memo<Vec<(Session, i64)>> {
    let (ws, live) = (use_workspace(), api::live());
    let views = LocalResource::new(move || {
        api::realm().track();
        ws.viewed.track();
        api::recent_sessions()
    });
    Memo::new(move |_| {
        let views = views.get().and_then(Result::ok).unwrap_or_default();
        live.sessions.with(|all| {
            views
                .iter()
                .filter(|v| !skip(&v.session_id))
                .filter_map(|v| all.iter().find(|s| s.id == v.session_id).map(|s| (s.clone(), v.at)))
                .take(limit)
                .collect()
        })
    })
}

/// A recently viewed session's state, as the Sessions list colours it:
/// needs you (an approval waits), else running, idle, starting or failed.
pub(crate) fn recent_state(s: &Session) -> (&'static str, &'static str) {
    if s.awaiting_approval {
        ("bg-amber-100 text-amber-800 dark:bg-amber-950 dark:text-amber-300", "needs you")
    } else {
        crate::util::status_look(s.display_status())
    }
}

/// How sessions are grouped: label keys, outermost first, kept in the head
/// (`session_grouping`) and shared by the sessions list and the rail.
#[derive(Clone, Copy)]
pub struct Grouping {
    levels: RwSignal<Vec<String>>,
}

impl Grouping {
    /// Loads it from the head (once, for the page: see [`use_grouping`]).
    pub fn load() -> Self {
        let levels = RwSignal::new(Vec::new());
        spawn_local(async move {
            if let Ok(v) = api::get_setting("session_grouping").await
                && let Ok(g) = serde_json::from_value::<c6_api::SessionGrouping>(v)
            {
                levels.set(g.levels);
            }
        });
        Grouping { levels }
    }

    fn save(&self, levels: Vec<String>) -> impl std::future::Future<Output = Result<(), String>> + use<> {
        self.levels.set(levels.clone());
        api::put_setting("session_grouping", serde_json::json!({ "levels": levels }))
    }

    /// Groups by `levels` now, saying so if the head couldn't keep it.
    fn choose(&self, levels: Vec<String>) {
        let notice = crate::util::Notice::get();
        let done = self.save(levels);
        spawn_local(async move {
            if let Err(e) = done.await {
                notice.show(format!("Couldn't keep the grouping: {e}"), None);
            }
        });
    }
}

pub fn use_grouping() -> Grouping {
    expect_context::<Grouping>()
}

/// The groups you folded in one view of the list (in this browser).
#[derive(Clone, Copy)]
struct Folded {
    key: &'static str,
    paths: RwSignal<Vec<String>>,
}

impl Folded {
    const PAGE: &str = "c6.sessions.folded";
    const RAIL: &str = "c6.rail.folded";

    fn load(key: &'static str) -> Self {
        let folded = storage_get(key).and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
        Folded { key, paths: RwSignal::new(folded) }
    }

    fn toggle(&self, path: &str) {
        self.paths.update(|c| {
            if let Some(i) = c.iter().position(|p| p == path) {
                c.remove(i);
            } else {
                c.push(path.to_owned());
            }
        });
        storage_set(self.key, &serde_json::to_string(&self.paths.get_untracked()).unwrap_or_default());
    }
}

/// A session's value for a label key: its first `key:` label (folders in
/// their `~/` form).
fn group_value(s: &Session, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    c6_api::labels::display_labels(&s.auto_labels).iter().chain(s.labels.iter()).find_map(|l| l.strip_prefix(&prefix).map(str::to_owned))
}

/// The sessions in nested groups, one level per key: most recently active
/// group first, sessions without the label last. `compact`: the rail's.
fn grouped(
    sessions: Vec<Session>,
    levels: &[String],
    path: String,
    folded: Folded,
    row: &dyn Fn(Session) -> AnyView,
    compact: bool,
) -> AnyView {
    let Some((key, rest)) = levels.split_first() else {
        return sessions.into_iter().map(row).collect_view().into_any();
    };
    let mut groups: Vec<(Option<String>, Vec<Session>)> = Vec::new();
    for s in sessions {
        let v = group_value(&s, key);
        match groups.iter_mut().find(|(g, _)| *g == v) {
            Some((_, list)) => list.push(s),
            None => groups.push((v, vec![s])),
        }
    }
    let latest = |list: &[Session]| list.iter().map(|s| s.updated_at).max().unwrap_or_default();
    groups.sort_by_key(|(v, list)| (v.is_none(), std::cmp::Reverse(latest(list))));
    groups
        .into_iter()
        .map(|(value, list)| {
            let here = format!("{path}{key}:{}\u{1f}", value.as_deref().unwrap_or(""));
            let (count, running) = (list.len(), list.iter().filter(|s| s.display_status() == SessionStatus::Running).count());
            let name = match &value {
                Some(v) => view! { <span class="font-mono">{v.clone()}</span> }.into_any(),
                None => view! { <span class="italic">{format!("no {key}")}</span> }.into_any(),
            };
            let is_folded = {
                let here = here.clone();
                Memo::new(move |_| folded.paths.with(|c| c.contains(&here)))
            };
            let toggle = {
                let here = here.clone();
                move |_| folded.toggle(&here)
            };
            let rest = rest.to_vec();
            // Drawn once; folding hides them.
            let rows = grouped(list, &rest, here.clone(), folded, row, compact);
            let (button, body) = if compact {
                (
                    "flex w-full items-center gap-1 rounded px-1 py-0.5 text-left text-[11px] text-muted hover:bg-zinc-200/60 dark:hover:bg-zinc-800/60",
                    "ml-1.5 border-l border-zinc-200 pl-1 dark:border-zinc-800",
                )
            } else {
                (
                    "flex w-full items-center gap-2 bg-zinc-50 py-1.5 pl-3 pr-4 text-left text-xs text-zinc-600 hover:bg-zinc-100 dark:bg-zinc-900/60 dark:text-zinc-400 dark:hover:bg-zinc-900",
                    "divide-y divide-zinc-200 border-t border-zinc-200 pl-5 dark:divide-zinc-800 dark:border-zinc-800",
                )
            };
            view! {
                <div>
                    <button class=button on:click=toggle title=format!("{key}: {}", value.clone().unwrap_or_else(|| format!("no {key}")))>
                        <span class="w-3 shrink-0 text-sm text-muted">{move || if is_folded.get() { crate::icons::chevron_right() } else { crate::icons::chevron_down() }}</span>
                        {(!compact).then(|| view! { <span class="shrink-0 text-muted">{format!("{key}:")}</span> })}
                        <span class="min-w-0 truncate">{name}</span>
                        <span class="ml-auto shrink-0 tabular-nums">{count}</span>
                        {(running > 0).then(|| view! {
                            <span class="shrink-0 text-blue-600 dark:text-blue-400">{if compact {
                                view! { {format!("· {running}")}{crate::icons::play()} }.into_any()
                            } else { format!("{running} running").into_any() }}</span>
                        })}
                    </button>
                    <div class=body class:hidden=move || is_folded.get()>
                        {rows}
                    </div>
                </div>
            }
        })
        .collect_view()
        .into_any()
}

/// "Group by": the levels (remove, or move one out a level) and a key to
/// add, from the labels your sessions have.
#[component]
pub fn GroupBar(grouping: Grouping) -> impl IntoView {
    let live = api::live();
    let error = RwSignal::new(None::<String>);
    let save = move |levels: Vec<String>| {
        let done = grouping.save(levels);
        spawn_local(async move { error.set(done.await.err()) });
    };
    let keys = Memo::new(move |_| {
        let mut keys: Vec<String> = ["repo", "host", "worker", "folder", "model"].map(str::to_owned).to_vec();
        live.sessions.with(|ss| {
            for s in ss {
                for l in &s.labels {
                    if let Some((k, _)) = l.split_once(':')
                        && !keys.iter().any(|x| x == k)
                    {
                        keys.push(k.to_owned());
                    }
                }
            }
        });
        keys
    });
    view! {
        <div class="flex flex-wrap items-center gap-2 text-xs text-muted">
            <span>"Group by"</span>
            {move || {
                let levels = grouping.levels.get();
                levels.iter().enumerate().map(|(i, k)| {
                    let (outer, gone) = (levels.clone(), levels.clone());
                    view! {
                        {(i > 0).then(|| view! { <span class="text-muted">{icon(Icon::ChevronRight, Size::S14)}</span> })}
                        <span class="inline-flex items-center gap-1 rounded bg-violet-100 px-1.5 py-0.5 font-mono text-[11px] text-violet-800 dark:bg-violet-950 dark:text-violet-300">
                            {(i > 0).then(|| view! {
                                <button title="Group by this one level further out" aria-label="Group by this one level further out" on:click=move |_| {
                                    let mut l = outer.clone();
                                    l.swap(i - 1, i);
                                    save(l);
                                }>{icon(Icon::ChevronLeft, Size::S14)}</button>
                            })}
                            {k.clone()}
                            <button title="Stop grouping by this" on:click=move |_| {
                                let mut l = gone.clone();
                                l.remove(i);
                                save(l);
                            }>{crate::icons::icon(crate::icons::Icon::X, crate::icons::Size::S14)}</button>
                        </span>
                    }
                }).collect_view()
            }}
            <select class=format!("{INPUT} w-auto! py-0.5 text-xs")
                prop:value=""
                on:change=move |ev| {
                    let k = event_target_value(&ev);
                    if !k.is_empty() {
                        let mut l = grouping.levels.get_untracked();
                        l.push(k);
                        save(l);
                    }
                }>
                <option value="">{move || if grouping.levels.with(Vec::is_empty) { "none" } else { "then by…" }}</option>
                {move || {
                    let chosen = grouping.levels.get();
                    keys.get().into_iter().filter(|k| !chosen.contains(k)).map(|k| { let v = k.clone(); view! { <option value=v>{k}</option> } }).collect_view()
                }}
            </select>
            {move || error.get().map(|e| view! { <span class="text-red-600">{e}</span> })}
        </div>
    }
}

/// A session's menu in a list (its row, a card, the dock): archive it
/// without opening it (offering its docs if any, otherwise with Undo);
/// and what the list adds (`dismiss`).
#[component]
pub(crate) fn SessionMenu(id: String, #[prop(optional)] dismiss: Option<Callback<()>>) -> impl IntoView {
    let archiver = crate::session_archive::SessionArchive::get();
    let id = StoredValue::new(id);
    let archive = Callback::new(move |()| {
        archiver.start(id.get_value());
    });
    let mut items = Vec::new();
    if let Some(d) = dismiss {
        items.push(("Dismiss", d));
    }
    items.push(("Archive", archive));
    view! { <crate::util::Menu items row=true label="Session actions" /> }
}

/// A row's state word in its colour: idle quiet, the rest in colour.
fn state_colour(word: &str) -> &'static str {
    match word {
        "running" => "font-medium text-blue-600 dark:text-blue-400",
        "needs you" | "starting" => "font-medium text-amber-600 dark:text-amber-400",
        "failed" => "font-medium text-red-600 dark:text-red-400",
        _ => "text-muted",
    }
}

/// A row's line of where it runs: its worker, its folder's last part and
/// its model.
fn where_line(worker: &str, s: &Session) -> AnyView {
    let place = match (&s.ssh, s.cwd.is_empty()) {
        (Some(t), empty) => {
            let target = if empty { t.clone() } else { format!("{t}:{}", last_part(&s.cwd)) };
            relationship_label(worker.to_owned(), target).into_any()
        }
        (None, empty) => {
            let folder = if empty { "scratch folder".to_owned() } else { last_part(&s.cwd) };
            [worker, folder.as_str()].into_iter().filter(|x| !x.is_empty()).collect::<Vec<_>>().join(" · ").into_any()
        }
    };
    let model = if s.model.is_empty() { String::new() } else { format!(" · {}", s.model) };
    view! { {place}{model} }.into_any()
}

/// A session in the list: its mark, title, state and when it was last
/// active; under the title where it runs (all of it on hover) and, if you
/// gave it any, its labels (with the aspects Settings → Appearance shows).
#[component]
fn SessionRow(s: Session, now: RwSignal<i64>, hover: Hover) -> impl IntoView {
    let id = s.id.clone();
    let live = api::live();
    let worker = live.session_worker(&s);
    let updated = s.updated_at;
    let word = recent_state(&s).1;
    let mark = crate::marks::fill(&crate::marks::of(&s));
    // Native hover text stays plain; `where_line` draws the visible relationship.
    let place = match (&s.ssh, s.cwd.is_empty()) {
        (Some(t), _) => format!("{worker} ⇄ {t}:{}", s.cwd),
        (None, true) => format!("{worker}: a scratch folder"),
        (None, false) => format!("{worker}:{}", s.cwd),
    };
    let place = if s.on_head { format!("{place}\n{}", api::HEAD_TITLE) } else { place };
    let labels = own_labels(&s).join(", ");
    let aspects = !crate::aspects::shown(&s).is_empty();
    let line = where_line(&worker, &s);
    let chips = s.clone();
    view! {
        <div class="group flex items-center gap-1 pr-1 hover:bg-zinc-100 @min-[40rem]/view:pr-2 dark:hover:bg-zinc-900"
            on:mouseenter=move |ev| {
                use wasm_bindgen::JsCast;
                if !window().match_media("(hover: hover)").ok().flatten().is_some_and(|m| m.matches()) { return; }
                if let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
                    hover.show(id.clone(), &el);
                }
            }
            on:mouseleave=move |_| hover.hide_soon()>
        <Go to=View::session(s.id.clone()) class="flex min-w-0 flex-1 items-center gap-3 py-2.5 pl-3 @min-[40rem]/view:pl-4" title="Open beside this list (⌘/Ctrl-click: new tile if not already open)">
            <span class="mt-1.5 h-2.5 w-2.5 shrink-0 self-start rounded-full" style=mark title="Its mark"></span>
            <div class="min-w-0 flex-1">
                <div class="truncate font-medium">{s.title.clone()}</div>
                <div class="truncate text-xs text-muted" title=place>{line}</div>
                {(!labels.is_empty() || aspects).then(|| view! {
                    <div class="flex min-w-0 items-center gap-2 text-xs text-muted">
                        {(!labels.is_empty()).then(|| view! { <span class="min-w-0 truncate" title=labels.clone()>{labels.clone()}</span> })}
                        <crate::aspects::AspectChips s=chips.clone() />
                    </div>
                })}
            </div>
            <div class="flex shrink-0 flex-col items-end gap-0.5 text-xs @min-[40rem]/view:flex-row @min-[40rem]/view:items-center @min-[40rem]/view:gap-0">
                <span class=state_colour(word)>{word}</span>
                <span class="text-muted @min-[40rem]/view:w-20 @min-[40rem]/view:text-right">{move || ago(now.get() - updated)}</span>
            </div>
        </Go>
        <PreviewButton id=s.id.clone() hover />
        <SessionMenu id=s.id.clone() />
        </div>
    }
}

/// The rail's sessions: every session (in this realm, with the label
/// filter's labels), grouped as the sessions list is, one narrow row each.
/// Hovering a row pops out its details, with its colour and "New like
/// this"; clicking reveals it, or opens it (⌘/Ctrl-click: a new tile if unopened).
#[component]
pub fn SessionsRail(open: RwSignal<bool>) -> impl IntoView {
    let live = api::live();
    let grouping = use_grouping();
    let folded = Folded::load(Folded::RAIL);
    let label_filter = crate::labels::use_filter().0;
    let query = RwSignal::new(String::new());
    let hover = Hover::new();
    let group_at = RwSignal::new(None::<(f64, f64)>);
    let like_this = RwSignal::new(None::<SessionSeed>);
    let sessions = Memo::new(move |_| {
        let (q, wanted, realm) = (query.get().to_lowercase(), label_filter.get(), api::realm().get());
        live.sessions
            .get()
            .into_iter()
            .filter(|s| crate::labels::matches(s, &wanted) && (realm.is_empty() || s.realm == realm))
            .filter(|s| {
                q.is_empty()
                    || s.title.to_lowercase().contains(&q)
                    || s.labels.iter().chain(s.auto_labels.iter()).any(|l| l.to_lowercase().contains(&q))
            })
            .collect::<Vec<_>>()
    });
    let small = "rounded px-1 font-normal normal-case tracking-normal hover:bg-zinc-200/60 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100";
    view! {
        <div class="flex items-center gap-1 pb-1 pl-1.5 pr-3 pt-2 text-[11px] font-semibold uppercase tracking-wide text-muted">
            <button class=crate::activity::FOLD title=move || if open.get() { "Fold" } else { "Unfold" } on:click=move |_| open.update(|o| *o = !*o)>
                {move || if open.get() { crate::icons::chevron_down() } else { crate::icons::chevron_right() }}
            </button>
            <span class="flex-1 pl-0.5">"Sessions"
                {move || (!open.get()).then(|| view! { <span class="ml-1.5 rounded-full bg-zinc-200 px-1.5 font-normal tabular-nums dark:bg-zinc-800">{sessions.with(Vec::len)}</span> })}
            </span>
            <button class=small title="Group by label keys (shared with the sessions list)"
                on:click=move |ev| group_at.set(Some(crate::workspace::below(&ev, 288.0)))>
                {move || match grouping.levels.get().as_slice() {
                    [] => "group".to_owned(),
                    levels => levels.join("›"),
                }}
                {crate::icons::chevron_down()}
            </button>
            <Go to=View::Sessions class=small title="All sessions, and a new one">"New"</Go>
        </div>
        <crate::workspace::Popover at=group_at class="w-72">
            <GroupBar grouping />
        </crate::workspace::Popover>
        <Show when=move || open.get()>
        <div class="px-1.5 pb-1">
            <input class="w-full rounded border border-zinc-200 bg-white px-1.5 py-0.5 text-xs dark:border-zinc-800 dark:bg-zinc-900"
                placeholder="Filter" prop:value=move || query.get() on:input=move |ev| query.set(event_target_value(&ev)) />
        </div>
        <div class="min-h-0 flex-1 overflow-y-auto px-1.5 pb-3" on:scroll=move |_| hover.hide_now()>
            {move || {
                let list = sessions.get();
                if list.is_empty() {
                    return view! { <p class="px-2 py-2 text-xs text-muted">"No sessions match."</p> }.into_any();
                }
                grouped(list, &grouping.levels.get(), String::new(), folded, &move |s| view! { <RailRow s hover /> }.into_any(), true)
            }}
        </div>
        </Show>
        <SessionPreview hover like_this />
        {move || like_this.get().map(|seed| view! { <NewSessionDialog seed on_close=move |()| like_this.set(None) /> })}
    }
}

/// Which rail row's details pop out, and where; it stays while the pointer
/// is on the row or the card, and goes a moment after it leaves both.
#[derive(Clone, Copy)]
struct Hover {
    shown: RwSignal<Option<(String, f64, f64)>>,
    generation: StoredValue<u64>,
    pinned: RwSignal<bool>,
}

impl Hover {
    fn new() -> Self {
        Hover { shown: RwSignal::new(None), generation: StoredValue::new(0), pinned: RwSignal::new(false) }
    }

    fn show(&self, id: String, row: &web_sys::Element) {
        if self.pinned.get_untracked() {
            return;
        }
        self.keep();
        let r = row.get_bounding_client_rect();
        let vh = window().inner_height().ok().and_then(|v| v.as_f64()).unwrap_or(800.0);
        // Beside the rail, or below a list row/button. Clamping a card to
        // the right edge of a wide list mustn't cover its own opener.
        let (x, y) = match row.closest("aside").ok().flatten() {
            Some(rail) => (rail.get_bounding_client_rect().right() + 6.0, r.top()),
            None => (r.left(), r.bottom() + 6.0),
        };
        self.shown.set(Some((id, x, y.min(vh - 340.0).max(8.0))));
    }

    /// The pointer came back: don't hide.
    fn keep(&self) {
        self.generation.try_update_value(|g| *g += 1);
    }

    fn hide_soon(&self) {
        self.keep();
        let Some(me) = self.generation.try_get_value() else { return };
        let this = *self;
        set_timeout(
            move || {
                let focused = document().active_element().is_some_and(|el| el.closest("[data-session-preview]").ok().flatten().is_some());
                if this.generation.try_get_value() == Some(me) && this.pinned.try_get() == Some(false) && !focused {
                    this.shown.set(None);
                }
            },
            std::time::Duration::from_millis(250),
        );
    }

    fn hide_now(&self) {
        self.keep();
        self.pinned.set(false);
        if self.shown.try_with_untracked(Option::is_some) == Some(true) {
            self.shown.set(None);
        }
    }
}

/// Touch and keyboard entry to the same card as the rail's hover.
#[component]
fn PreviewButton(id: String, hover: Hover) -> impl IntoView {
    view! {
        <button class=icons::BAR_BUTTON title="Session details" aria-label="Session details"
            on:click=move |ev| {
                use wasm_bindgen::JsCast;
                hover.hide_now();
                if let Some(el) = ev.current_target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) {
                    hover.show(id.clone(), &el);
                    hover.pinned.set(true);
                }
            }>{icons::info()}</button>
    }
}

#[component]
fn SessionPreview(hover: Hover, like_this: RwSignal<Option<SessionSeed>>) -> impl IntoView {
    let live = api::live();
    let pop = StoredValue::new(escapable(move || hover.shown.with(Option::is_some), move || hover.hide_now()));
    // A heartbeat may change the row, not the identity of this preview.
    // In particular it mustn't recreate its picker or discard focus.
    let shown = move || {
        hover.shown.get().filter(|(id, _, _)| live.sessions.with(|ss| ss.iter().any(|s| s.id == *id))).into_iter().collect::<Vec<_>>()
    };
    view! {
        <For each=shown
            key=|(id, _, _)| id.clone() children=move |(id, _, _)| {
                let s = live.sessions.with_untracked(|ss| ss.iter().find(|s| s.id == id).cloned()).expect("previewed row");
                view! { <RailCard s hover like_this pop=pop.get_value() /> }
            } />
    }
}

#[component]
fn RailRow(s: Session, hover: Hover) -> impl IntoView {
    let id = s.id.clone();
    let ws = use_workspace();
    let number = {
        let id = id.clone();
        Memo::new(move |_| ws.tile_number(&id))
    };
    let colour = crate::marks::of(&s);
    let glyphs = s.clone();
    let state = if s.awaiting_approval {
        "h-1.5 w-1.5 shrink-0 animate-pulse rounded-full bg-amber-500"
    } else {
        match s.display_status() {
            SessionStatus::Running => "h-1.5 w-1.5 shrink-0 animate-pulse rounded-full bg-blue-500",
            SessionStatus::Failed => "h-1.5 w-1.5 shrink-0 rounded-full bg-red-500",
            _ => "hidden",
        }
    };
    view! {
        <div
            on:mouseenter=move |ev| {
                if !window().match_media("(hover: hover)").ok().flatten().is_some_and(|m| m.matches()) { return; }
                if let Some(el) = ev.current_target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok()) {
                    hover.show(id.clone(), &el);
                }
            }
            on:mouseleave=move |_| hover.hide_soon()
        >
            <div class="flex items-center">
            <Go to=View::session(s.id.clone()) class="flex min-w-0 flex-1 items-center gap-1.5 rounded px-1 py-0.5 text-[13px] hover:bg-zinc-200/60 dark:hover:bg-zinc-800/60">
                <span class="h-2.5 w-1 shrink-0 rounded-sm" style=crate::marks::fill(&colour)></span>
                <span class="min-w-0 flex-1 truncate">{s.title.clone()}</span>
                <span class="flex shrink-0 items-center gap-0.5"><crate::aspects::AspectGlyphs s=glyphs /></span>
                <span class=state></span>
                {move || number.get().map(crate::activity::tile_badge)}
            </Go>
            <PreviewButton id=s.id.clone() hover />
            </div>
        </div>
    }
}

/// A rail row's details: all of its title, its state, where it runs, its
/// labels and colour, and what you can do with it.
#[component]
fn RailCard(s: Session, hover: Hover, like_this: RwSignal<Option<SessionSeed>>, pop: String) -> impl IntoView {
    let ws = use_workspace();
    let live = api::live();
    let now = clock();
    let worker = live.session_worker(&s);
    let card = NodeRef::<leptos::html::Div>::new();
    Effect::new(move || {
        if !hover.pinned.get() || card.get().is_none() {
            return;
        }
        request_animation_frame(move || {
            use wasm_bindgen::JsCast;
            if let Some(el) = card
                .try_get_untracked()
                .flatten()
                .and_then(|el| el.query_selector("[aria-label=\"Change session mark\"]").ok().flatten())
                .and_then(|el| el.dyn_into::<web_sys::HtmlElement>().ok())
            {
                let _ = el.focus();
            }
        });
    });
    let id = StoredValue::new(s.id.clone());
    let current = Memo::new({
        let fallback = s.clone();
        move |_| live.sessions.with(|ss| ss.iter().find(|s| s.id == id.get_value()).cloned()).unwrap_or_else(|| fallback.clone())
    });
    let place_session = s.clone();
    let place = move || match (&place_session.ssh, place_session.cwd.is_empty()) {
        (Some(t), _) => relationship_label(String::new(), format!("{t}:{}", place_session.cwd)).into_any(),
        (None, true) => "scratch workspace".into_any(),
        (None, false) => c6_api::labels::display_labels(&place_session.auto_labels)
            .iter()
            .find_map(|l| l.strip_prefix("folder:").map(str::to_owned))
            .unwrap_or_else(|| place_session.cwd.clone())
            .into_any(),
    };
    let act = "rounded-md border border-zinc-200 px-2 py-1 text-xs hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800";
    view! {
        <leptos::portal::Portal>
        <div node_ref=card data-session-preview data-pop=pop.clone() class="fixed z-40 w-72 space-y-2 overflow-y-auto rounded-lg border border-zinc-200 bg-white p-3 text-sm shadow-xl dark:border-zinc-700 dark:bg-zinc-900"
            style=move || {
                let (_, x, y) = hover.shown.get().unwrap_or_default();
                format!("left:clamp(8px,{x}px,max(8px,calc(100vw - 18rem - 8px)));top:min({y}px,8px + max(0px,100dvh - 340px));max-width:calc(100vw - 16px);max-height:calc(100dvh - min({y}px,8px + max(0px,100dvh - 340px)) - 8px)")
            }
            on:mouseenter=move |_| hover.keep()
            on:focusin=move |_| hover.keep()
            on:focusout=move |_| hover.hide_soon()
            on:mouseleave=move |_| hover.hide_soon()>
            <div class="flex items-start gap-2">
                <div class="min-w-0 flex-1 break-words font-medium">{move || current.get().title}</div>
                {move || view! { <StatusPill status=current.get().display_status() /> }}
                <button class=icons::BAR_BUTTON title="Close session details" aria-label="Close session details" on:click=move |_| hover.hide_now()>{icons::x()}</button>
            </div>
            {s.awaiting_approval.then(|| view! { <p class="text-xs font-medium text-amber-600">"A tool call waits for your approval"</p> })}
            <div class="space-y-0.5 text-xs text-muted">
                <div class="truncate"><span title=s.on_head.then_some(api::HEAD_TITLE)>{worker.clone()}</span> " · " {move || current.get().model}</div>
                <div class="break-all font-mono">{place()}</div>
                <div>{move || format!("active {}", ago(now.get() - current.get().updated_at))}</div>
            </div>
            {(!s.labels.is_empty() || !crate::labels::shown_auto(&s.auto_labels).is_empty()).then(|| view! {
                <div class="flex flex-wrap gap-1">
                    {crate::labels::shown_auto(&s.auto_labels).into_iter().map(|l| view! { <crate::labels::LabelChip label=l auto=true stay=true /> }).collect_view()}
                    {s.labels.iter().cloned().map(|l| view! { <crate::labels::LabelChip label=l stay=true /> }).collect_view()}
                </div>
            })}
            <crate::aspects::AspectChips s=s.clone() />
            <crate::marks::SessionSwatch session=s.clone() />
            <div class="flex flex-wrap gap-1.5 pt-1">
                <button class=act on:click=move |_| { hover.hide_now(); ws.open(None, View::session(id.get_value()), How::Beside) }>"Open"</button>
                <button class=act on:click=move |_| { hover.hide_now(); ws.open(None, View::session(id.get_value()), How::OwnTile) }>"New tile"</button>
                {(!s.observed && s.instance.is_empty()).then(|| view! {
                    <button class=act title="Start a new, empty session with this one's worker, folder, model and labels (you can change them first)"
                        on:click=move |_| { let seed = SessionSeed::from_session(&current.get_untracked()); hover.hide_now(); like_this.set(Some(seed)) }>"New like this"</button>
                })}
            </div>
        </div>
        </leptos::portal::Portal>
    }
}

/// What a new session starts from when it is made like another one: where
/// it runs, its model and its labels (the agents, skills, hooks and
/// commands that apply follow from those).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionSeed {
    pub worker_id: String,
    /// The folder on the worker, or on the SSH host.
    pub cwd: String,
    pub ssh: Option<String>,
    pub connection_id: String,
    pub model: String,
    pub effort: String,
    pub labels: Vec<String>,
    /// It ran in a git worktree of `cwd`: the new one gets its own.
    pub worktree: bool,
    pub realm: String,
    /// The session it was made from: the new one joins its work.
    pub from: String,
}

impl SessionSeed {
    pub fn from_session(s: &Session) -> Self {
        SessionSeed {
            worker_id: s.worker_id.to_string(),
            cwd: s.cwd.clone(),
            ssh: s.ssh.clone(),
            connection_id: s.connection_id.clone(),
            model: s.model.clone(),
            effort: s.effort.clone().unwrap_or_default(),
            labels: s.labels.clone(),
            worktree: false,
            realm: s.realm.clone(),
            from: s.id.clone(),
        }
    }
}

/// A popup with the new-session form, filled in from `seed`: change what
/// you like and start it. The new session opens in a tile of its own.
#[component]
pub fn NewSessionDialog(seed: SessionSeed, #[prop(into)] on_close: Callback<()>) -> impl IntoView {
    let connections = crate::resources::connections();
    let seed = StoredValue::new(seed);
    view! {
        // Over every tile, wherever it was opened from: inside a tile it
        // could show behind the others.
        <leptos::portal::Portal>
            <div class="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/40 px-4 py-16" on:click=move |_| on_close.run(())
                on:keydown=move |ev| {
                    // Not the session's Esc, which stops its turn.
                    if ev.key() == "Escape" {
                        ev.stop_propagation();
                        on_close.run(());
                    }
                }>
                <div class="w-full max-w-3xl space-y-3 rounded-lg bg-white p-4 shadow-xl outline-none dark:bg-zinc-900" tabindex="-1" on:click=|ev| ev.stop_propagation()>
                    <div class="flex items-center gap-2">
                        <h2 class="flex-1 font-medium">"New session like this one"</h2>
                        <button class=format!("{BUTTON_QUIET} text-xs text-muted") title="Close (Esc)" on:click=move |_| on_close.run(())>{crate::icons::x()}</button>
                    </div>
                    <p class="text-xs text-muted">"A new, empty session with this one's worker, folder, model and labels (so the same agents, skills and hooks). Change anything before starting it."</p>
                    <Suspense fallback=|| view! { <p class="text-sm text-muted">"Loading…"</p> }>
                        {move || connections.get().map(|r| match r {
                            Ok(conns) if conns.is_empty() => view! { <p class="text-sm text-muted">"Add an LLM connection in Settings to start a session."</p> }.into_any(),
                            Ok(conns) => view! { <SessionForm connections=conns seed=seed.get_value() on_started=on_close on_cancel=on_close /> }.into_any(),
                            Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                        })}
                    </Suspense>
                </div>
            </div>
        </leptos::portal::Portal>
    }
}

/// A name for a template made from a session: its folder's last part.
fn template_name(seed: &SessionSeed) -> String {
    let base = seed.cwd.trim_end_matches('/').rsplit('/').next().unwrap_or_default();
    let name: String = base.chars().map(|c| if c.is_alphanumeric() || "_-.".contains(c) { c } else { '-' }).collect();
    if name.trim_matches('-').is_empty() { "session".into() } else { name }
}

/// Saves a session's place and model as a template (docs/remote-subagents.md
/// §4): its worker, folder, SSH host, connection, model, effort and labels,
/// so agents (with *Agents may start it*) or you can start sessions like it.
#[component]
pub fn SaveTemplateDialog(seed: SessionSeed, #[prop(into)] on_close: Callback<()>) -> impl IntoView {
    let live = api::live();
    let worker = live.worker_name(&seed.worker_id);
    let name = RwSignal::new(template_name(&seed));
    let description = RwSignal::new(String::new());
    let prompt = RwSignal::new(String::new());
    let agents = RwSignal::new(true);
    let worktree = RwSignal::new(seed.worktree);
    let status = RwSignal::new(None::<Result<String, String>>);
    let (place_worker, place_target, place_folder) = (worker.clone(), seed.ssh.clone(), seed.cwd.clone());
    let place = move || match &place_target {
        Some(t) => relationship_label(place_worker.clone(), format!("{t}:{place_folder}")).into_any(),
        None if place_folder.is_empty() => format!("{place_worker}: a scratch folder").into_any(),
        None => format!("{place_worker}:{place_folder}").into_any(),
    };
    let model = if seed.model.is_empty() { "by routing rules".to_owned() } else { seed.model.clone() };
    let seed = StoredValue::new(seed);
    let save = Callback::new(move |_: leptos::ev::MouseEvent| {
        let seed = seed.get_value();
        let template = c6_api::Template {
            name: name.get_untracked().trim().to_owned(),
            description: description.get_untracked().trim().to_owned(),
            workers: vec![worker.clone()],
            ssh: seed.ssh.clone().into_iter().collect(),
            hosts: Vec::new(),
            cwd: seed.cwd.clone(),
            connection: seed.connection_id.clone(),
            model: seed.model.clone(),
            effort: seed.effort.clone(),
            labels: seed.labels.clone(),
            prompt: prompt.get_untracked(),
            enabled: true,
            worktree: worktree.get_untracked() && !seed.cwd.is_empty() && seed.ssh.is_none(),
            agents: agents.get_untracked(),
            title: String::new(),
            ..Default::default()
        };
        spawn_local(async move {
            let result = async {
                let mut list = api::templates().await?;
                if list.iter().any(|t| t.name == template.name) {
                    return Err(format!("a template is already named {:?}", template.name));
                }
                let name = template.name.clone();
                list.push(template);
                api::put_setting("templates", serde_json::to_value(&list).map_err(|e| e.to_string())?).await?;
                Ok(format!("Saved as {name} (Settings → Templates)"))
            }
            .await;
            let done = result.is_ok();
            status.set(Some(result));
            if done {
                on_close.run(());
            }
        });
    });
    view! {
        // Over every tile, wherever it was opened from: inside a tile it
        // could show behind the others.
        <leptos::portal::Portal>
            <div class="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/40 px-4 py-16" on:click=move |_| on_close.run(())
                on:keydown=move |ev| if ev.key() == "Escape" { ev.stop_propagation(); on_close.run(()); }>
                <div class="w-full max-w-xl space-y-3 rounded-lg bg-white p-4 text-sm shadow-xl outline-none dark:bg-zinc-900" tabindex="-1" on:click=|ev| ev.stop_propagation()>
                    <div class="flex items-center gap-2">
                        <h2 class="flex-1 font-medium">"Save as template"</h2>
                        <button class=format!("{BUTTON_QUIET} text-xs text-muted") title="Close (Esc)" on:click=move |_| on_close.run(())>{crate::icons::x()}</button>
                    </div>
                    <p class="text-xs text-muted">"Sessions from it run where this one does, on its model and with its labels."</p>
                    <div class="space-y-0.5 text-xs text-muted">
                        <div class="break-all font-mono">{place.clone()}</div>
                        <div>{model.clone()}</div>
                    </div>
                    <label class="block space-y-0.5">
                        <span class="text-xs text-muted">"Name"</span>
                        <input class=format!("{INPUT} py-1 font-mono text-xs") prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) />
                    </label>
                    <label class="block space-y-0.5">
                        <span class="text-xs text-muted">"Description (agents read it to choose the template)"</span>
                        <input class=format!("{INPUT} py-1") prop:value=move || description.get() on:input=move |ev| description.set(event_target_value(&ev)) />
                    </label>
                    <label class="block space-y-0.5">
                        <span class="text-xs text-muted">"Prompt (optional: standing instructions; a subagent's task fills {{input}}, or follows)"</span>
                        <textarea class=format!("{INPUT} min-h-20 font-mono text-xs") prop:value=move || prompt.get() on:input=move |ev| prompt.set(event_target_value(&ev))></textarea>
                    </label>
                    <label class="flex min-h-8 items-start gap-2 pointer-coarse:min-h-11">
                        <input type="checkbox" class=crate::components::CHECKBOX prop:checked=move || agents.get() on:change=move |ev| agents.set(event_target_checked(&ev)) />
                        <span>"Agents may start it"<span class="block text-xs text-muted">"Any session's agent can start it as a remote subagent, here, whatever worker it runs on"</span></span>
                    </label>
                    <label class="flex min-h-8 items-start gap-2 pointer-coarse:min-h-11">
                        <input type="checkbox" class=crate::components::CHECKBOX prop:checked=move || worktree.get() on:change=move |ev| worktree.set(event_target_checked(&ev)) />
                        <span>"New git worktree"<span class="block text-xs text-muted">"Each session works in a new worktree of the folder, on a new c6/… branch"</span></span>
                    </label>
                    <div class="flex items-center justify-end gap-3">
                        {move || status.get().map(|r| match r {
                            Ok(m) => view! { <span class="text-xs text-emerald-600">{m}</span> }.into_any(),
                            Err(e) => view! { <span class="text-xs text-red-600">{e}</span> }.into_any(),
                        })}
                        <button class=BUTTON on:click=move |ev| save.run(ev)>"Save"</button>
                    </div>
                </div>
            </div>
        </leptos::portal::Portal>
    }
}

/// The new-session form in a popup, with your usual choices and a first
/// message written for you (docs/conversations.md §3.2).
#[component]
pub fn SessionDialog(
    #[prop(into)] title: String,
    #[prop(into)] note: String,
    first_message: String,
    #[prop(optional, into)] on_session: Option<Callback<Session>>,
    #[prop(into)] on_close: Callback<()>,
    /// Start from this session's worker, folder, model and labels.
    #[prop(optional)]
    seed: Option<SessionSeed>,
    /// The work it joins (docs/work.md §4), and what it was started from.
    #[prop(optional)]
    work: Option<String>,
    #[prop(optional)] work_refs: Vec<WorkRef>,
    /// The to-do of `work` it does.
    #[prop(optional)]
    todo: Option<String>,
) -> impl IntoView {
    let connections = crate::resources::connections();
    let first = StoredValue::new(first_message);
    let (seed, work, work_refs) = (StoredValue::new(seed), StoredValue::new(work), StoredValue::new(work_refs));
    let todo = StoredValue::new(todo.unwrap_or_default());
    view! {
        // Over every tile, wherever it was opened from: inside a tile it
        // could show behind the others.
        <leptos::portal::Portal>
            <div class="fixed inset-0 z-50 flex items-start justify-center overflow-y-auto bg-black/40 px-4 py-16" on:click=move |_| on_close.run(())
                on:keydown=move |ev| if ev.key() == "Escape" { ev.stop_propagation(); on_close.run(()); }>
                <div class="w-full max-w-3xl space-y-3 rounded-lg bg-white p-4 text-left shadow-xl outline-none dark:bg-zinc-900" tabindex="-1" on:click=|ev| ev.stop_propagation()>
                    <div class="flex items-center gap-2">
                        <h2 class="flex-1 font-medium">{title.clone()}</h2>
                        <button class=format!("{BUTTON_QUIET} text-xs text-muted") title="Close (Esc)" on:click=move |_| on_close.run(())>{crate::icons::x()}</button>
                    </div>
                    <p class="text-xs text-muted">{note.clone()}</p>
                    <Suspense fallback=|| view! { <p class="text-sm text-muted">"Loading…"</p> }>
                        {move || connections.get().map(|r| match r {
                            Ok(conns) if conns.is_empty() => view! { <p class="text-sm text-muted">"Add an LLM connection in Settings to start a session."</p> }.into_any(),
                            Ok(conns) => match seed.get_value() {
                                Some(seed) => view! {
                                    <SessionForm connections=conns seed first_message=first.get_value() on_session=on_session.unwrap_or(Callback::new(|_| ()))
                                        work=work.get_value().unwrap_or_default() work_refs=work_refs.get_value() todo=todo.get_value() on_started=on_close on_cancel=on_close />
                                }.into_any(),
                                None => view! {
                                    <SessionForm connections=conns first_message=first.get_value() on_session=on_session.unwrap_or(Callback::new(|_| ()))
                                        work=work.get_value().unwrap_or_default() work_refs=work_refs.get_value() todo=todo.get_value() on_started=on_close on_cancel=on_close />
                                }.into_any(),
                            },
                            Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                        })}
                    </Suspense>
                </div>
            </div>
        </leptos::portal::Portal>
    }
}

/// The new-session form: the prompt first, then one line of where and how
/// it runs (worker, folder, model, effort), a Worktree chip, Change for the
/// fields (in place on a desktop, a sheet on a phone), Template ▾ on the
/// Sessions page, and Send. With a `seed` it starts from those values
/// instead of the ones remembered in this browser, and remembers none of
/// its own.
#[component]
fn SessionForm(
    connections: Vec<Connection>,
    #[prop(optional)] seed: Option<SessionSeed>,
    /// After the session started (and opened).
    #[prop(optional, into)]
    on_started: Option<Callback<()>>,
    /// Shows a Cancel button.
    #[prop(optional, into)]
    on_cancel: Option<Callback<()>>,
    /// The first message, written for you (still yours to change).
    #[prop(optional, into)]
    first_message: Option<String>,
    /// With the session it started.
    #[prop(optional, into)]
    on_session: Option<Callback<Session>>,
    /// The work it joins (docs/work.md §4): an item's id, "new" or "none";
    /// empty: an agent's guess.
    #[prop(optional, into)]
    work: String,
    /// What it was started from, attached to its work with it.
    #[prop(optional)]
    work_refs: Vec<WorkRef>,
    /// The to-do of `work` it does (empty: none).
    #[prop(optional, into)]
    todo: String,
) -> impl IntoView {
    let live = api::live();
    let (ws, col) = (use_workspace(), use_column());
    let seeded = seed.is_some();
    let seed = seed.unwrap_or_default();
    // Radio groups and datalists are per document: each form its own names.
    static FORMS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = FORMS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let (where_name, recent_id) = (format!("c6-where-{n}"), format!("c6-ssh-recent-{n}"));

    let worker = RwSignal::new(if seeded { seed.worker_id.clone() } else { storage_get("c6.worker").unwrap_or_default() });
    let cwd = RwSignal::new(if seeded && seed.ssh.is_none() { seed.cwd.clone() } else { String::new() });
    let picking = RwSignal::new(false);
    // Or on an SSH host, through the worker: the target and a folder there.
    let remote = RwSignal::new(if seeded { seed.ssh.is_some() } else { storage_get("c6.where").as_deref() == Some("ssh") });
    let ssh = RwSignal::new(seed.ssh.clone().unwrap_or_default());
    let remote_dir = RwSignal::new(if seed.ssh.is_some() { seed.cwd.clone() } else { String::new() });
    // Hosts from the inventory, offered by name.
    let inventory = LocalResource::new(api::hosts);
    let recent_ssh = move || -> Vec<String> {
        storage_get(&format!("c6.ssh.{}", worker.get())).map(|s| c6_api::labels::parse_labels(&s)).unwrap_or_default()
    };
    let worker_online = move || {
        let w = worker.get();
        live.workers.with(|ws| ws.iter().any(|x| x.id.to_string() == w && x.status == WorkerStatus::Online))
    };
    // Empty: "Auto", your routing rules choose the connection and model.
    // A seed's connection, if it is still there.
    let seeded_connection = (seeded && connections.iter().any(|x| x.id == seed.connection_id)).then(|| seed.connection_id.clone());
    let remembered_connection = seeded_connection
        .clone()
        .or_else(|| storage_get("c6.connection").filter(|c| c.is_empty() || connections.iter().any(|x| &x.id == c)));
    let connection = RwSignal::new(remembered_connection.clone().unwrap_or_else(|| connections[0].id.clone()));
    let has_routes = RwSignal::new(false);
    spawn_local(async move {
        let routes = api::get_setting("routes").await.ok().and_then(|v| v.as_array().cloned()).unwrap_or_default();
        let any = routes.iter().any(|r| r["enabled"].as_bool() != Some(false));
        has_routes.set(any);
        // First time here with rules set up: start on Auto.
        if any && remembered_connection.is_none() {
            connection.set(String::new());
        }
    });
    let auto = move || connection.get().is_empty();
    let model = RwSignal::new(if seeded_connection.is_some() { seed.model.clone() } else { String::new() });
    let effort = RwSignal::new(if seeded { seed.effort.clone() } else { storage_get("c6.effort").unwrap_or_default() });
    let worktree =
        RwSignal::new(if seeded { seed.worktree && seed.ssh.is_none() } else { storage_get("c6.worktree").as_deref() == Some("1") });
    // A fresh form keeps what you typed until the session starts.
    let saved_draft = !seeded && first_message.is_none();
    let prompt = match first_message {
        Some(m) => RwSignal::new(m),
        None if !seeded => crate::util::draft("new"),
        None => RwSignal::new(String::new()),
    };
    // Your labels for the session (remembered per folder).
    let labels = RwSignal::new(seed.labels.join(", "));
    let seed_realm = StoredValue::new(seed.realm.clone());
    // The work it belongs to: yours to pick (a seed's session's, if any),
    // else an agent picks.
    let work_items = LocalResource::new(|| api::work_list(false));
    // Active projects, for a quick session in one (docs/projects.md §3.1).
    let quick_projects = LocalResource::new(|| api::projects_list(false));
    let work_pick = RwSignal::new(work);
    if !seed.from.is_empty() && work_pick.get_untracked().is_empty() {
        let from = seed.from.clone();
        spawn_local(async move {
            if let Ok(ws) = api::work_of(format!("session:{from}")).await
                && let Some(w) = ws.first()
                && work_pick.get_untracked().is_empty()
            {
                work_pick.set(w.id.clone());
            }
        });
    }
    let work_refs = StoredValue::new(work_refs);
    let (todo, todo_work) = (StoredValue::new(todo), StoredValue::new(work_pick.get_untracked()));
    // What its work starts with (docs/dev-starts.md §4): its template, the
    // worker it picks (or its worktree's) and why, and the branch.
    let plan = RwSignal::new(None::<c6_api::starts::StartPlan>);
    let worktree_options = RwSignal::new(None::<c6_api::starts::WorktreeOptions>);
    let branch = RwSignal::new(String::new());
    let pending = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    // Images and files for the first message: pasted, dropped or picked.
    let (attachments, draft_note, draft_ready) = crate::util::draft_attachments(saved_draft.then_some("new"));
    let picker = NodeRef::<leptos::html::Input>::new();
    let conns = StoredValue::new(connections);
    // Effort only where the connection passes it on to the model.
    // On Auto, the rule sets it.
    let effort_supported = move || conns.with_value(|cs| cs.iter().find(|c| c.id == connection.get()).is_some_and(|c| c.supports_effort));

    let chosen_by_hand = RwSignal::new(seeded);
    let ask_plan = move |other: String| {
        let w = work_pick.get_untracked();
        if w.is_empty() || matches!(w.as_str(), "new" | "none" | "guess") {
            plan.set(None);
            worktree_options.set(None);
            return;
        }
        let q = c6_api::starts::StartQuery {
            work: w,
            todo: todo.get_value(),
            worker: other,
            text: prompt.get_untracked(),
            ..Default::default()
        };
        spawn_local(async move {
            let Ok(p) = api::start_plan(q).await else { return };
            if p.template.is_empty() && !p.reuse && !p.offline {
                plan.try_set(None);
                return;
            }
            chosen_by_hand.try_set(true);
            plan.try_set(Some(p.clone()));
            if let Some(id) = p.worker_id {
                worker.try_set(id.to_string());
            }
            remote.try_set(false);
            cwd.try_set(p.cwd.clone());
            worktree.try_set(p.worktree);
            branch.try_set(p.options.branch.clone());
            worktree_options.try_set(p.worktree.then(|| p.options.clone()));
            if !p.connection_id.is_empty() {
                connection.try_set(p.connection_id.clone());
                model.try_set(p.model.clone());
            }
            if !p.effort.is_empty() {
                effort.try_set(p.effort.clone());
            }
            let mut l = c6_api::labels::parse_labels(&labels.get_untracked());
            l.extend(p.labels.iter().filter(|x| !l.contains(x)).cloned().collect::<Vec<_>>());
            labels.try_set(l.join(", "));
            if prompt.get_untracked().trim().is_empty() && !p.prompt.is_empty() {
                prompt.try_set(p.prompt.clone());
            }
        });
    };
    // Asked again when you pick other work, or another worker than the one
    // its worktree is on.
    Effect::new(move || {
        work_pick.track();
        ask_plan(String::new());
    });
    Effect::new(move || {
        let w = worker.get();
        let moved =
            plan.with_untracked(|p| p.as_ref().is_some_and(|p| (p.reuse || p.offline) && p.worker_id.is_none_or(|id| id.to_string() != w)));
        if moved && !w.is_empty() {
            ask_plan(w);
        }
    });

    // Pick a worker once the fleet loads: the last one used if it is online,
    // otherwise an online one. A worker the user picks by hand is kept, and
    // so is a seed's.
    Effect::new(move || {
        // The memex is a node, not a worker: it runs no sessions.
        let ws: Vec<c6_api::Worker> = live.workers.get().into_iter().filter(|w| w.role != "memex").collect();
        if chosen_by_hand.get_untracked() {
            return;
        }
        let current = worker.get_untracked();
        let status = |id: &str| ws.iter().find(|w| w.id.to_string() == id).map(|w| w.status);
        if status(&current) == Some(WorkerStatus::Online) {
            return;
        }
        let pick = ws
            .iter()
            .find(|w| w.status == WorkerStatus::Online)
            .or_else(|| (status(&current).is_none()).then(|| ws.iter().find(|w| w.status != WorkerStatus::Revoked)).flatten());
        if let Some(w) = pick {
            worker.set(w.id.to_string());
        }
    }); // Remember the last directory per worker (a seed's folder first).
    Effect::new(move |first: Option<()>| {
        let w = worker.get();
        if first.is_none() && seeded {
            return;
        }
        // The plan's folder on the plan's worker.
        if let Some(p) = plan.get_untracked().filter(|p| p.worker_id.is_some_and(|id| id.to_string() == w)) {
            cwd.set(p.cwd);
            return;
        }
        let remembered = storage_get(&format!("c6.cwd.{w}")).unwrap_or_default();
        cwd.set(remembered.clone());
        // Forget a remembered folder that no longer exists there.
        if !remembered.is_empty() {
            spawn_local(async move {
                if api::fs_list(w.clone(), remembered.clone(), false).await.is_err() && cwd.get_untracked() == remembered {
                    cwd.set(String::new());
                }
            });
        }
    });
    // A seed's labels stay whatever the folder.
    if !seeded {
        Effect::new(move || {
            let (w, c) = (worker.get(), cwd.get());
            labels.set(storage_get(&format!("c6.labels.{w}.{c}")).unwrap_or_default());
        });
    }
    // Default model per connection (a seed's model for its connection).
    let keep_model = seeded_connection.is_some();
    Effect::new(move |first: Option<()>| {
        let c = connection.get();
        if first.is_none() && keep_model {
            return;
        }
        if c.is_empty() {
            model.set(String::new());
            return;
        }
        let default = conns.with_value(|cs| cs.iter().find(|x| x.id == c).and_then(|x| x.default_model.clone()));
        model.set(storage_get(&format!("c6.model.{c}")).or(default).unwrap_or_default());
    });

    // The Sessions page's box (not a popup's) can run a template instead:
    // every realm's (an app's are in its realm). The box's text is its
    // {{input}}, or its prompt is shown when it takes none.
    let page = on_cancel.is_none();
    let templates = LocalResource::new(move || async move { if page { api::all_templates().await } else { Ok(Vec::new()) } });
    let templates = Signal::derive(move || {
        templates.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|t| t.template.enabled).collect::<Vec<_>>()
    });
    let template = RwSignal::new(None::<c6_api::RealmTemplate>);
    let takes_input = move || template.with(|t| t.as_ref().is_some_and(|t| t.template.prompt.contains("{{input}}")));
    let note = RwSignal::new(None::<String>);
    let filter = crate::labels::use_filter().0;
    // One session opens beside; a fleet run, mission control filtered to it.
    let run_template = move |t: c6_api::RealmTemplate| {
        let input = if takes_input() { prompt.get_untracked() } else { String::new() };
        let used = !input.is_empty();
        pending.set(true);
        error.set(None);
        note.set(None);
        spawn_local(async move {
            let ran = api::run_template(t.template.name.clone(), input, Some(t.realm.clone())).await;
            pending.try_set(false);
            match ran {
                Ok(r) => {
                    let mut started = format!("Started {} session{}", r.sessions.len(), if r.sessions.len() == 1 { "" } else { "s" });
                    if !r.errors.is_empty() {
                        started.push_str(&format!(" ({} could not start: {})", r.errors.len(), r.errors.join("; ")));
                    }
                    note.try_set(Some(started));
                    if used {
                        prompt.try_set(String::new());
                    }
                    if let [one] = r.sessions.as_slice() {
                        match col {
                            Some(c) => c.open(View::session(one.id.clone()), How::Beside),
                            None => ws.open(None, View::session(one.id.clone()), How::Beside),
                        }
                    } else {
                        filter.set(vec![format!("run:{}", r.run_id)]);
                        ws.open(None, View::Mission, How::Beside);
                    }
                }
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
        });
    };

    let submit = move || {
        if pending.get_untracked() || !draft_ready.get_untracked() {
            return;
        }
        if let Some(t) = template.get_untracked() {
            run_template(t);
            return;
        }
        let on_ssh = remote.get_untracked();
        let target = ssh.get_untracked().trim().to_owned();
        if on_ssh && target.is_empty() {
            error.set(Some("Enter the SSH host (user@host, or an alias from the worker's ~/.ssh/config)".into()));
            return;
        }
        if !on_ssh && cwd.get_untracked().is_empty() && worktree.get_untracked() {
            error.set(Some("A git worktree needs a folder".into()));
            return;
        }
        let Ok(worker_id) = worker.get_untracked().parse() else {
            error.set(Some("Pick a worker".into()));
            return;
        };
        let req = CreateSession {
            worker_id,
            cwd: if on_ssh { remote_dir.get_untracked().trim().to_owned() } else { cwd.get_untracked() },
            ssh: on_ssh.then(|| target.clone()),
            connection_id: connection.get_untracked(),
            model: model.get_untracked(),
            title: None,
            prompt: Some(prompt.get_untracked()).filter(|p| !p.trim().is_empty()),
            attachments: attachments.get_untracked(),
            labels: c6_api::labels::parse_labels(&labels.get_untracked()),
            effort: Some(effort.get_untracked()).filter(|e| !e.is_empty() && effort_supported()),
            worktree: worktree.get_untracked() && !on_ssh,
            // As its work's template says, with the branch as you left it.
            worktree_options: worktree_options
                .get_untracked()
                .filter(|_| worktree.get_untracked() && !on_ssh)
                .map(|o| c6_api::starts::WorktreeOptions { branch: branch.get_untracked().trim().to_owned(), ..o }),
            // A seed's realm; else the realm you are looking at (else home).
            realm: if seeded { Some(seed_realm.get_value()) } else { Some(crate::api::realm().get_untracked()) }.filter(|r| !r.is_empty()),
            // "An agent picks" asks for its guess.
            work: Some(work_pick.get_untracked()).map(|w| if w.is_empty() { "guess".to_owned() } else { w }),
            work_refs: work_refs.get_value(),
            // Its to-do, while it still joins that work.
            todo: Some(todo.get_value()).filter(|t| !t.is_empty() && work_pick.get_untracked() == todo_work.get_value()),
        };
        if on_ssh {
            // Recent targets of this worker, newest first.
            let mut recent = recent_ssh();
            recent.retain(|t| *t != target);
            recent.insert(0, target.clone());
            recent.truncate(10);
            storage_set(&format!("c6.ssh.{}", req.worker_id), &recent.join(","));
        }
        // A seeded form leaves the main form's defaults alone.
        if !seeded {
            storage_set("c6.where", if on_ssh { "ssh" } else { "worker" });
            storage_set("c6.worktree", if worktree.get_untracked() { "1" } else { "0" });
            storage_set("c6.effort", &effort.get_untracked());
            storage_set("c6.worker", &req.worker_id.to_string());
            storage_set(&format!("c6.cwd.{}", req.worker_id), &req.cwd);
            storage_set(&format!("c6.labels.{}.{}", req.worker_id, req.cwd), &req.labels.join(", "));
            storage_set("c6.connection", &req.connection_id);
            if !req.connection_id.is_empty() {
                storage_set(&format!("c6.model.{}", req.connection_id), &req.model);
            }
        }
        pending.set(true);
        spawn_local(async move {
            let created = api::create_session(req).await;
            pending.try_set(false);
            match created {
                // The new session takes the form's tile (Back returns to
                // the form); from a popup, a tile of its own.
                Ok(s) => {
                    if saved_draft {
                        // Clear storage, not signals: opening the session
                        // disposes this form and its pending render effects.
                        crate::util::forget_draft("new");
                    }
                    if let Some(f) = on_session {
                        f.run(s.clone());
                    }
                    let how = if on_cancel.is_some() { How::OwnTile } else { How::Instead };
                    match col {
                        Some(c) => c.open(View::session(s.id), how),
                        None => ws.open(None, View::session(s.id), how),
                    }
                    // Closing a dialog disposes this form: on the next tick,
                    // once what it set has been drawn.
                    if let Some(f) = on_started {
                        set_timeout(
                            move || {
                                f.try_run(());
                            },
                            std::time::Duration::ZERO,
                        );
                    }
                }
                Err(e) => {
                    error.try_set(Some(e));
                }
            }
        });
    };
    let prompt_box = NodeRef::<leptos::html::Textarea>::new();
    // It grows with the text (a template's prompt, shown, too).
    crate::util::autosize(prompt_box, move || {
        prompt.track();
        template.track();
    });
    // In a popup, start typing the first message right away.
    if on_cancel.is_some() {
        Effect::new(move || {
            if let Some(el) = prompt_box.get() {
                let _ = el.focus();
            }
        });
    }

    // Where and how it runs, changed only when needed: in place on a
    // desktop, a sheet on a phone. Esc puts it away.
    let changing = RwSignal::new(false);
    let fields_pop = escapable(
        move || changing.get(),
        move || {
            changing.try_set(false);
        },
    );
    let template_at = RwSignal::new(None::<(f64, f64)>);
    // A template that takes no input: its prompt, shown in the box.
    let fixed =
        move || template.with(|t| t.as_ref().filter(|t| !t.template.prompt.contains("{{input}}")).map(|t| t.template.prompt.clone()));
    let ready = move || template.with(Option::is_some) || prompt.with(|p| !p.trim().is_empty());
    let touch = crate::util::touch_first();
    let worker_label = move || {
        let w = worker.get();
        live.workers.with(|ws| ws.iter().find(|x| x.id.to_string() == w).map(|x| x.name.clone())).unwrap_or_else(|| "no worker".to_owned())
    };
    let model_label = move || {
        if auto() {
            return "auto".to_owned();
        }
        let m = model.get();
        if m.trim().is_empty() {
            conns.with_value(|cs| cs.iter().find(|c| c.id == connection.get()).map(|c| c.name.clone())).unwrap_or_default()
        } else {
            m
        }
    };
    let line = move |short: bool| {
        let on_ssh = remote.get();
        let target = ssh.get().trim().to_owned();
        let folder = if on_ssh { remote_dir.get().trim().to_owned() } else { cwd.get() };
        let effort = if effort_supported() { effort.get() } else { String::new() };
        let target = if target.is_empty() { "an SSH host".to_owned() } else { target };
        let worker = worker_label();
        let model = model_label();
        let target = on_ssh.then_some(target.as_str());
        (summary(&worker, target, &folder, &model, &effort, short), summary_label(&worker, target, &folder, &model, &effort, short))
    };
    let template_label = |t: &c6_api::RealmTemplate| {
        if t.realm == "home" { t.template.name.clone() } else { format!("{} ({})", t.template.name, t.realm) }
    };
    let template_key = |t: &c6_api::RealmTemplate| format!("{}/{}", t.realm, t.template.name);
    let pick_template = move |t: Option<c6_api::RealmTemplate>| {
        template.set(t);
        changing.set(false);
        template_at.set(None);
        note.set(None);
    };
    let tool = "inline-flex h-6 shrink-0 items-center gap-1 rounded-md px-1.5 text-xs text-soft hover:bg-zinc-100 hover:text-zinc-900 pointer-coarse:h-8 dark:hover:bg-zinc-800 dark:hover:text-zinc-100";
    let send_title = move || {
        if template.with(Option::is_some) {
            "Run the template"
        } else if !ready() {
            "Start an empty session"
        } else if touch {
            "Start session"
        } else {
            "Start session (Enter)"
        }
    };

    view! {
        {move || picking.get().then(|| view! {
            <crate::files::FolderPicker
                worker_id=worker.get_untracked()
                start=cwd.get_untracked()
                on_select=Callback::new(move |p: String| { cwd.set(p); picking.set(false); })
                on_cancel=Callback::new(move |_| picking.set(false))
            />
        })}
        <div class="space-y-2">
        <section class="rounded-xl border border-zinc-200 bg-white focus-within:border-zinc-400 dark:border-zinc-800 dark:bg-zinc-900 dark:focus-within:border-zinc-600"
            on:dragover=|ev| ev.prevent_default()
            on:drop=move |ev| {
                let ev: web_sys::Event = ev.into();
                if crate::session::js_has_files(&ev) {
                    ev.prevent_default();
                    crate::session::add_attachments(ev, attachments, error);
                }
            }>
            {move || (!draft_ready.get()).then(|| view! { <p class="px-2 pt-2 text-xs text-muted" role="status">"Restoring draft attachments…"</p> })}
            {move || draft_note.get().map(|note| view! { <p class="px-2 pt-2 text-xs text-muted" role="status">{note}</p> })}
            {move || (!attachments.with(Vec::is_empty)).then(|| view! {
                <div class="flex flex-wrap gap-2 px-2 pt-2">
                    {attachments.get().into_iter().enumerate().map(|(i, a)| {
                        let image = a.mime.starts_with("image/");
                        view! {
                            <div class="flex items-center gap-1.5 rounded-md border border-zinc-200 bg-white p-1 pr-2 text-xs dark:border-zinc-700 dark:bg-zinc-900">
                                {if image {
                                    view! { <img class="h-10 w-10 rounded object-cover" src=format!("data:{};base64,{}", a.mime, a.data) /> }.into_any()
                                } else {
                                    view! { <span class="px-1">{crate::icons::icon(crate::icons::Icon::File, crate::icons::Size::S14)}</span> }.into_any()
                                }}
                                <span class="max-w-40 truncate">{a.name.clone()}</span>
                                <button type="button" class="text-muted hover:text-zinc-900 dark:hover:text-zinc-100" title="Remove"
                                    on:click=move |_| attachments.update(|v| { if i < v.len() { v.remove(i); } })>{crate::icons::x()}</button>
                            </div>
                        }
                    }).collect_view()}
                </div>
            })}
            <textarea node_ref=prompt_box rows="2"
                on:paste=move |ev| {
                    let ev: web_sys::Event = ev.into();
                    if crate::session::js_has_files(&ev) {
                        ev.prevent_default();
                        crate::session::add_attachments(ev, attachments, error);
                    }
                }
                class=format!("block w-full bg-transparent px-3 pb-1 pt-2.5 text-sm outline-none disabled:text-muted {PLACEHOLDER} {GROWS}")
                placeholder=move || match template.get() {
                    Some(t) if t.template.prompt.contains("{{input}}") => format!("Input for {} (its {{{{input}}}})", t.template.name),
                    Some(t) => format!("{} takes no input: Send runs it", t.template.name),
                    None if seeded => "First message (optional: leave it empty to start an empty session)".to_owned(),
                    None => "What should the agent do?".to_owned(),
                }
                prop:value=move || fixed().unwrap_or_else(|| prompt.get())
                prop:disabled=move || fixed().is_some()
                on:input=move |ev| prompt.set(event_target_value(&ev))
                on:keydown=move |ev| {
                    if crate::util::sends(&ev) { ev.prevent_default(); submit(); }
                }
            ></textarea>
            <div class="flex min-w-0 flex-wrap items-center gap-1.5 px-2 pb-2">
                {move || fixed().is_none().then(|| view! {
                    <button type="button" class="flex size-6.5 shrink-0 items-center justify-center rounded-md text-muted hover:bg-zinc-100 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
                        title="Attach images or files (or paste / drop them)" aria-label="Attach images or files"
                        on:click=move |_| { if let Some(i) = picker.get_untracked() { i.click(); } }>
                        {crate::icons::icon(crate::icons::Icon::Plus, crate::icons::Size::S14)}
                    </button>
                })}
                <input node_ref=picker type="file" multiple class="hidden"
                    on:change=move |ev| crate::session::add_attachments(ev, attachments, error) />
                // A desktop's summary line, then its buttons.
                {move || match template.get() {
                    Some(t) => view! {
                        <span class="hidden min-w-0 truncate text-xs text-muted md:inline" title=t.template.description.clone()>
                            "Template: " <span class="text-soft">{template_label(&t)}</span>
                            {(!t.template.description.is_empty()).then(|| format!(" · {}", t.template.description))}
                        </span>
                        <button class=format!("{tool} hidden md:inline-flex") title="Don't use the template" aria-label="Don't use the template"
                            on:click=move |_| pick_template(None)>{icons::x()}</button>
                    }.into_any(),
                    None => view! {
                        <span class="hidden min-w-0 truncate text-xs text-muted md:inline" title=move || line(false).0>{move || line(false).1}</span>
                    }.into_any(),
                }}
                // A phone's one chip, opening every field in a sheet.
                <button data-pop=fields_pop.clone() aria-haspopup="dialog" aria-expanded=move || changing.get().to_string()
                    class="inline-flex h-8 min-w-0 items-center gap-1 rounded-md border border-zinc-200 px-2 text-xs text-soft md:hidden dark:border-zinc-700"
                    title="Where and how it runs" on:click=move |_| changing.update(|c| *c = !*c)>
                    <span class="min-w-0 truncate">{move || match template.with(|t| t.as_ref().map(template_label)) {
                        Some(label) => label.into_any(),
                        None => line(true).1,
                    }}</span>
                    <span class="text-muted">{crate::icons::chevron_down()}</span>
                </button>
                <label
                    title="A new git worktree: a c6/… branch and a separate checkout for this session"
                    class=move || if remote.get() || template.with(Option::is_some) { "hidden".to_owned() } else {
                        format!("inline-flex min-h-8 shrink-0 cursor-pointer items-center gap-1 rounded-md border px-1.5 text-xs pointer-coarse:min-h-11 {}",
                            if worktree.get() { "border-sky-300 bg-sky-50 text-sky-800 dark:border-sky-800 dark:bg-sky-950 dark:text-sky-300" } else { "border-zinc-300 text-soft hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800" })
                    }>
                    <input type="checkbox" class=crate::components::CHECKBOX prop:checked=move || worktree.get()
                        on:change=move |ev| worktree.set(event_target_checked(&ev)) />
                    "Worktree"
                </label>
                <button data-pop=fields_pop.clone() aria-expanded=move || changing.get().to_string()
                    class=move || if template.with(Option::is_some) { "hidden".to_owned() } else { format!("{tool} hidden md:inline-flex") }
                    title="Change where and how it runs" on:click=move |_| changing.update(|c| *c = !*c)>
                    {move || if changing.get() { "Done" } else { "Change" }}
                </button>
                {move || (page && !templates.with(Vec::is_empty)).then(|| view! {
                    <button class=format!("{tool} hidden md:inline-flex") aria-haspopup="menu" title="Run a template: it sets where and how its sessions run"
                        on:click=move |ev| template_at.set(Some(below(&ev, 320.0)))>
                        "Template" <span class="text-muted">{crate::icons::chevron_down()}</span>
                    </button>
                })}
                <span class="flex-1"></span>
                {on_cancel.map(|c| view! { <button class=tool on:click=move |_| c.run(())>"Cancel"</button> })}
                <button class=move || format!("{SEND} {SEND_SHAPE} {}", if ready() { SEND_READY } else { SEND_EMPTY })
                    title=send_title aria-label=send_title prop:disabled=move || pending.get() || !draft_ready.get() on:click=move |_| submit()>
                    {move || if pending.get() {
                        view! { <span class="h-2.5 w-2.5 animate-spin rounded-full border-2 border-current border-t-transparent"></span> }.into_any()
                    } else {
                        send_icon().into_any()
                    }}
                </button>
            </div>
            <Popover at=template_at class="w-80">
                {move || template.with(Option::is_some).then(|| view! {
                    <button class="block w-full rounded-md px-2 py-1.5 text-left hover:bg-zinc-100 dark:hover:bg-zinc-800" on:click=move |_| pick_template(None)>
                        "None: a session as set here"
                    </button>
                })}
                {move || templates.get().into_iter().map(|t| {
                    let on = template.with(|x| x.as_ref().map(template_key)) == Some(template_key(&t));
                    let (label, description) = (template_label(&t), t.template.description.clone());
                    view! {
                        <button class=format!("block w-full rounded-md px-2 py-1.5 text-left hover:bg-zinc-100 dark:hover:bg-zinc-800 {}", if on { "bg-zinc-100 dark:bg-zinc-800" } else { "" })
                            on:click=move |_| pick_template(Some(t.clone()))>
                            <span class="block font-medium">{label}</span>
                            {(!description.is_empty()).then(|| view! { <span class="block text-xs text-muted">{description}</span> })}
                        </button>
                    }
                }).collect_view()}
            </Popover>
            {move || plan.get().map(|p| {
                let tone = if p.offline { "text-amber-700 dark:text-amber-400" } else { "text-muted" };
                view! {
                    <div class=format!("flex flex-wrap items-center gap-x-3 gap-y-1 px-1 text-xs {tone}") data-start-plan="">
                        {(!p.template.is_empty()).then(|| view! { <span>"Template " <span class="font-medium text-soft">{p.template.clone()}</span></span> })}
                        <span data-start-why="">{p.why.clone()}</span>
                        {move || (worktree.get() && worktree_options.with(Option::is_some)).then(|| view! {
                            <label class="inline-flex items-center gap-1">"New branch"
                                <input class="w-56 rounded border border-zinc-200 bg-transparent px-1.5 py-0.5 font-mono text-xs dark:border-zinc-700" data-start-branch=""
                                    prop:value=move || branch.get() on:input=move |ev| branch.set(event_target_value(&ev)) />
                            </label>
                            {worktree_options.with(|o| o.as_ref().map(|o| {
                                let base = if o.base.is_empty() { "HEAD".to_owned() } else { o.base.clone() };
                                format!("from {base}{}", if o.fetch { " (fetched)" } else { "" })
                            }))}
                        })}
                        {p.reuse.then(|| view! { <span class="font-mono">{format!("in its worktree, on {}", p.options.branch)}</span> })}
                    </div>
                }
            })}
            <div class:hidden=move || !changing.get()>
                <div class="fixed inset-0 z-40 bg-black/30 md:hidden" on:click=move |_| changing.set(false)></div>
                <div data-pop=fields_pop
                    class="phone-sheet fixed inset-x-0 bottom-0 z-50 flex max-h-[85dvh] flex-col rounded-t-xl border-t border-zinc-200 bg-white text-sm shadow-xl md:static md:z-auto md:max-h-none md:rounded-t-none md:rounded-b-xl md:shadow-none dark:border-zinc-800 dark:bg-zinc-900">
                    <h2 class="shrink-0 border-b border-zinc-200 px-4 py-3 font-medium md:hidden dark:border-zinc-800">"New session: where and how it runs"</h2>
                    <div class="min-h-0 flex-1 space-y-3 overflow-y-auto px-4 py-4 md:overflow-visible md:px-3 md:py-3">
                        {move || template.with(Option::is_some).then(|| view! {
                            <p class="text-xs text-muted">"The template sets where and how its sessions run."</p>
                        })}
                        <div class="space-y-3" class:hidden=move || template.with(Option::is_some)>
                            <div class="grid grid-cols-1 gap-3 @min-[40rem]/view:grid-cols-[minmax(0,1fr)_minmax(0,2fr)]">
                                <label class="block min-w-0 text-xs text-muted">
                                    "Worker"
                                    <select class=format!("mt-1 {INPUT}") on:change=move |ev| { chosen_by_hand.set(true); worker.set(event_target_value(&ev)) } prop:value=move || worker.get()>
                                        {move || live.workers.get().into_iter().filter(|w| w.status != WorkerStatus::Revoked && w.role != "memex").map(|w| {
                                            let label = if w.status == WorkerStatus::Online { w.name.clone() } else { format!("{} (offline)", w.name) };
                                            view! { <option value=w.id.to_string()>{label}</option> }
                                        }).collect_view()}
                                    </select>
                                </label>
                                <div class="min-w-0 text-xs text-muted">
                                    <div class="flex flex-wrap items-center gap-x-3 gap-y-1">
                                        <label class="flex items-center gap-1"><input type="radio" name=where_name.clone() prop:checked=move || !remote.get() on:change=move |_| remote.set(false) />"Folder on the worker"</label>
                                        <label class="flex items-center gap-1" title="Work on another machine over SSH, using the worker's keys and ~/.ssh/config">
                                            <input type="radio" name=where_name prop:checked=move || remote.get() on:change=move |_| remote.set(true) />"SSH host, through the worker"
                                        </label>
                                    </div>
                                    <div class="mt-1 flex flex-wrap gap-2" class:hidden=move || !remote.get()>
                                        <input class=format!("{INPUT} {PLACEHOLDER} w-auto! min-w-0 flex-1 basis-40 font-mono") list=recent_id.clone() placeholder="user@host or an ssh_config alias"
                                            prop:value=move || ssh.get() on:input=move |ev| ssh.set(event_target_value(&ev)) />
                                        <datalist id=recent_id>
                                            {move || inventory.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|h| h.host.enabled).map(|h| {
                                                let label = format!("{} · {}", h.host.target, h.host.labels.join(", "));
                                                view! { <option value=h.host.name.clone() label=label></option> }
                                            }).collect_view()}
                                            {move || recent_ssh().into_iter().map(|t| view! { <option value=t></option> }).collect_view()}
                                        </datalist>
                                        <input class=format!("{INPUT} {PLACEHOLDER} w-auto! min-w-0 flex-1 basis-28 font-mono") placeholder="folder there (~)"
                                            prop:value=move || remote_dir.get() on:input=move |ev| remote_dir.set(event_target_value(&ev)) />
                                    </div>
                                    <div class="mt-1 flex gap-2" class:hidden=move || remote.get()>
                                        <button class=format!("{INPUT} min-w-0 flex-1 truncate text-left font-mono disabled:opacity-60")
                                            disabled=move || !worker_online()
                                            title=move || if worker_online() { "Choose a folder on the worker" } else { "The worker is offline" }
                                            on:click=move |_| picking.set(true)>
                                            {move || if cwd.get().is_empty() { "No folder: a scratch workspace (choose one…)".to_owned() } else { cwd.get() }}
                                        </button>
                                        {move || (!cwd.get().is_empty()).then(|| view! {
                                            <button class="shrink-0 rounded-md px-2 text-xs text-muted hover:bg-zinc-200/60 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
                                                title="Work in a scratch workspace instead of a folder" aria-label="No folder" on:click=move |_| cwd.set(String::new())>{icons::x()}</button>
                                        })}
                                    </div>
                                </div>
                            </div>
                            <div class="min-w-0 text-xs text-muted">
                                "Connection and model"
                                <div class="mt-1 flex flex-wrap items-center gap-2">
                                    <ModelPicker connection model effort auto=has_routes />
                                </div>
                                {move || auto().then(|| view! {
                                    <p class="mt-1">"The connection, model and effort come from the first rule in Settings → Model routing that matches this session's labels."</p>
                                })}
                            </div>
                            <div class="grid grid-cols-1 gap-3 @min-[40rem]/view:grid-cols-[minmax(0,2fr)_minmax(0,1fr)]">
                                <label class="block min-w-0 text-xs text-muted">
                                    "Labels"
                                    <input class=format!("mt-1 font-mono {INPUT} {PLACEHOLDER}") placeholder="optional, e.g. client:acme, spike"
                                        prop:value=move || labels.get() on:input=move |ev| labels.set(event_target_value(&ev)) />
                                    <span class="mt-0.5 block">"Repo, folder, worker and model are added automatically."</span>
                                </label>
                                <label class="block min-w-0 text-xs text-muted" title="What you mean to finish that this session serves (docs/work.md)">
                                    "Work"
                                    <select class=format!("mt-1 {INPUT}")
                                        prop:value=move || { work_items.track(); quick_projects.track(); work_pick.get() }
                                        on:change=move |ev| work_pick.set(event_target_value(&ev))>
                                        <option value="">"An agent picks (or starts one)"</option>
                                        <option value="new">"New work"</option>
                                        <option value="none">"Not part of any work"</option>
                                        {move || work_items.get().and_then(Result::ok).unwrap_or_default().into_iter().map(|w| {
                                            view! { <option value=w.id.clone()>{w.fields.title.clone()}</option> }
                                        }).collect_view()}
                                        {move || {
                                            let projects: Vec<_> = quick_projects.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|p| p.fields.status == "active").collect();
                                            (!projects.is_empty()).then(|| view! {
                                                <optgroup label="A quick task, no work item">
                                                    {projects.into_iter().map(|p| view! {
                                                        <option value=format!("project:{}", p.id)>{format!("Just this project: {}", p.fields.title)}</option>
                                                    }).collect_view()}
                                                </optgroup>
                                            })
                                        }}
                                    </select>
                                </label>
                            </div>
                        </div>
                        // A phone has no Template ▾: it's here.
                        {move || (page && !templates.with(Vec::is_empty)).then(|| view! {
                            <label class="block min-w-0 text-xs text-muted md:hidden">
                                "Template"
                                <select class=format!("mt-1 {INPUT}")
                                    prop:value=move || template.with(|t| t.as_ref().map(template_key).unwrap_or_default())
                                    on:change=move |ev| {
                                        let k = event_target_value(&ev);
                                        template.set(templates.get_untracked().into_iter().find(|t| template_key(t) == k));
                                        note.set(None);
                                    }>
                                    <option value="">"None: a session as set above"</option>
                                    {templates.get().into_iter().map(|t| view! { <option value=template_key(&t)>{template_label(&t)}</option> }).collect_view()}
                                </select>
                                {move || template.get().filter(|t| !t.template.description.is_empty()).map(|t| view! {
                                    <span class="mt-0.5 block">{t.template.description}</span>
                                })}
                            </label>
                        })}
                    </div>
                    <div class="notice-safe shrink-0 border-t border-zinc-200 px-4 pt-3 md:hidden dark:border-zinc-800">
                        <button class=format!("{BUTTON} w-full py-2") on:click=move |_| changing.set(false)>"Done"</button>
                    </div>
                </div>
            </div>
        </section>
        {move || {
            let is_cc = conns.with_value(|cs| cs.iter().any(|c| c.id == connection.get() && c.kind == "claude_code"));
            let w = worker.get();
            let missing = live.workers.with(|ws| ws.iter().any(|x| x.id.to_string() == w && x.status == WorkerStatus::Online && !x.capabilities.iter().any(|c| c == "claude_code")));
            (is_cc && missing && template.with(Option::is_none)).then(|| view! {
                <p class="text-sm text-amber-700 dark:text-amber-400">"This worker doesn't have the claude CLI installed (or it isn't on the worker's PATH; set C6_CLAUDE_BIN)."</p>
            })
        }}
        {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
        {move || note.get().map(|m| view! { <p class="text-sm text-emerald-600 dark:text-emerald-400">{m}</p> })}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_takes_where_and_how_a_session_runs() {
        let s: Session = serde_json::from_value(serde_json::json!({
            "id": "s1", "worker_id": "00000000-0000-0000-0000-000000000001", "title": "t", "cwd": "/src/app",
            "connection_id": "anthropic", "model": "claude-x", "status": "idle", "error": null, "effort": "high",
            "created_at": 0, "updated_at": 0, "last_seq": 0, "labels": ["client:acme"], "auto_labels": ["repo:app"],
            "ssh": "admin@web1", "realm": "work"
        }))
        .unwrap();
        let seed = SessionSeed::from_session(&s);
        assert_eq!(seed.worker_id, "00000000-0000-0000-0000-000000000001");
        assert_eq!((seed.cwd.as_str(), seed.ssh.as_deref()), ("/src/app", Some("admin@web1")));
        assert_eq!((seed.connection_id.as_str(), seed.model.as_str(), seed.effort.as_str()), ("anthropic", "claude-x", "high"));
        // Only your labels: the automatic ones are made again for the new session.
        assert_eq!(seed.labels, vec!["client:acme".to_owned()]);
        assert_eq!(seed.realm, "work");
        assert!(!seed.worktree);
    }

    #[test]
    fn a_recent_row_says_running_idle_or_needs_you() {
        let mut s: Session = serde_json::from_value(serde_json::json!({
            "id": "s1", "worker_id": "00000000-0000-0000-0000-000000000001", "title": "t", "cwd": "",
            "connection_id": "", "model": "", "status": "running", "created_at": 0, "updated_at": 0, "last_seq": 0
        }))
        .unwrap();
        assert_eq!(recent_state(&s).1, "running");
        assert_eq!(recent_state(&s).0, crate::util::status_look(SessionStatus::Running).0, "the Sessions list's colours");
        s.status = SessionStatus::Idle;
        assert_eq!(recent_state(&s).1, "idle");
        s.awaiting_approval = true;
        assert_eq!(recent_state(&s), ("bg-amber-100 text-amber-800 dark:bg-amber-950 dark:text-amber-300", "needs you"));
    }

    fn session(id: &str, title: &str, updated_at: i64) -> Session {
        serde_json::from_value(serde_json::json!({
            "id": id, "worker_id": "00000000-0000-0000-0000-000000000001", "title": title, "cwd": "",
            "connection_id": "", "model": "", "status": "idle", "created_at": 0, "updated_at": updated_at, "last_seq": 0
        }))
        .unwrap()
    }

    fn ids(list: &[Session]) -> Vec<&str> {
        list.iter().map(|s| s.id.as_str()).collect()
    }

    #[test]
    fn recently_viewed_comes_first_then_the_rest_by_last_activity() {
        let list = vec![session("a", "alpha", 10), session("b", "Zeta", 50), session("c", "Mid", 30), session("d", "beta", 40)];
        // Viewed c, then a (a the newest view); b and d not viewed.
        let viewed = vec![("a".to_owned(), 900), ("c".to_owned(), 800), ("gone".to_owned(), 999)];
        assert_eq!(ids(&sorted(list.clone(), Sort::Viewed, &viewed)), ["a", "c", "b", "d"]);
        assert_eq!(Sort::default(), Sort::Viewed);
        // Nothing viewed: by last activity alone.
        assert_eq!(ids(&sorted(list.clone(), Sort::Viewed, &[])), ["b", "d", "c", "a"]);
        assert_eq!(ids(&sorted(list.clone(), Sort::Activity, &viewed)), ["b", "d", "c", "a"]);
        // Titles whatever their case.
        assert_eq!(ids(&sorted(list, Sort::Title, &viewed)), ["a", "d", "c", "b"]);
    }

    #[test]
    fn your_labels_are_not_the_automatic_ones() {
        let mut s = session("a", "t", 0);
        s.labels = ["client:acme", "spike", "repo:github.com/acme/app", "host:web1", "worker:thalassa", "folder:~/x", "model:opus"]
            .map(str::to_owned)
            .to_vec();
        s.auto_labels = vec!["repo:github.com/acme/app".to_owned(), "worker:thalassa".to_owned()];
        assert_eq!(own_labels(&s), ["client:acme", "spike"]);
        // A template run's labels are the head's, not yours.
        s.labels = ["ops", "template:disk-check", "run:r1", "started-by:s0"].map(str::to_owned).to_vec();
        assert_eq!(own_labels(&s), ["ops"]);
        s.labels.clear();
        assert!(own_labels(&s).is_empty(), "automatic labels alone: no labels line");
    }

    #[test]
    fn the_sliders_dot_shows_while_a_choice_is_not_the_default() {
        assert!(!non_default("", &[], Sort::Viewed, &[]));
        assert!(non_default("running", &[], Sort::Viewed, &[]));
        assert!(non_default("", &["client:acme".to_owned()], Sort::Viewed, &[]));
        assert!(non_default("", &[], Sort::Title, &[]));
        assert!(non_default("", &[], Sort::Activity, &[]));
        assert!(non_default("", &[], Sort::Viewed, &["worker".to_owned()]));
    }

    #[test]
    fn show_takes_sessions_by_their_state_word() {
        let mut s = session("a", "t", 0);
        assert!(shows(&s, "") && shows(&s, "idle") && !shows(&s, "running") && !shows(&s, "needs"));
        s.status = SessionStatus::Running;
        assert!(shows(&s, "running") && !shows(&s, "idle"));
        s.awaiting_approval = true;
        assert!(shows(&s, "needs") && !shows(&s, "running"), "needs you, not running");
    }

    #[test]
    fn the_summary_says_where_and_how_in_one_line() {
        assert_eq!(
            summary("thalassa", None, "/home/z/projects/carbon6", "opus", "high", false),
            "on thalassa · ~/projects/carbon6 · opus · high"
        );
        assert_eq!(summary("thalassa", None, "/home/z/projects/carbon6", "opus", "high", true), "thalassa · carbon6 · opus");
        assert_eq!(summary("thalassa", None, "", "auto", "", false), "on thalassa · scratch folder · auto");
        assert_eq!(summary("w", Some("admin@web1"), "/srv/app", "m", "", true), "w · ⇄ admin@web1:app · m");
        assert_eq!(home_form("/Users/v/code"), "~/code");
        assert_eq!(home_form("/root"), "~");
        assert_eq!(home_form("/srv/app"), "/srv/app");
        assert_eq!(last_part("/srv/app/"), "app");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn visible_remote_labels_draw_the_relation_while_native_titles_stay_text() {
        let arrow = icon(Icon::ArrowLeftRight, Size::S14).to_html();
        for short in [true, false] {
            let html = summary_label("worker", Some("admin@web1"), "/srv/app", "model", "high", short).to_html();
            assert_eq!(html.matches("<svg").count(), 1, "keep the relationship icon: {html}");
            assert!(html.contains(&arrow), "{html}");
            assert!(!html.contains('⇄'), "the visible label is drawn: {html}");
            assert!(html.find("worker").unwrap() < html.find("<svg").unwrap(), "{html}");
            assert!(html.find("<svg").unwrap() < html.find("admin@web1").unwrap(), "{html}");
            assert!(html.contains(if short { "admin@web1:app" } else { "admin@web1:/srv/app" }), "{html}");
            assert!(summary("worker", Some("admin@web1"), "/srv/app", "model", "high", short).contains('⇄'));
        }
        let local = summary_label("worker", None, "/srv/app", "model", "high", false).to_html();
        assert_eq!(local, summary("worker", None, "/srv/app", "model", "high", false));

        let mut s = session("s1", "Remote session", 0);
        s.ssh = Some("admin@web1".into());
        s.cwd = "/srv/app".into();
        let row = where_line("worker", &s).to_html();
        assert!(row.contains(&arrow), "{row}");
        assert!(row.contains("admin@web1:app"), "{row}");
        assert!(!row.contains('⇄'), "{row}");
    }

    #[test]
    fn grouping_by_a_key_puts_it_outermost() {
        let levels = ["repo".to_owned(), "worker".to_owned()];
        assert_eq!(outermost(&levels, "folder"), ["folder", "worker"]);
        assert_eq!(outermost(&levels, "worker"), ["worker"]);
        assert_eq!(outermost(&[], "worker"), ["worker"]);
    }
}
