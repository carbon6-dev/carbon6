//! A session's aspects (docs/marks.md): its worker, its work, its project,
//! its realm, its repo and its model, each value in its mark and its aspect's shape.
//! A tile's bar shows the ones its realm chose (Settings → Appearance), as
//! shapes, with names when the tile is wide; pointing at one dims the tiles
//! that don't share it, and clicking it shows the value, the other sessions
//! with it, and its palette.

use c6_api::Session;
use c6_api::marks::{ASPECTS, SessionFacets};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::marks::{Glyph, Shape, ValuePalette, live_mark};
use crate::workspace::{Go, Popover, View, below};

/// One value of one of a session's aspects.
#[derive(Clone, Debug, PartialEq)]
pub struct Aspect {
    pub aspect: &'static str,
    /// `<aspect>:<value>`, the key its mark has.
    pub key: String,
    /// What it's called.
    pub label: String,
}

impl Aspect {
    pub fn shape(&self) -> Shape {
        Shape::of(self.aspect)
    }

    fn view(&self) -> Option<View> {
        let (_, value) = self.key.split_once(':')?;
        match self.aspect {
            "worker" => Some(View::Worker { id: value.to_owned() }),
            "work" => Some(View::Work { id: value.to_owned() }),
            "project" => Some(View::Projects { id: value.to_owned() }),
            _ => None,
        }
    }

    fn what(&self) -> &'static str {
        match self.aspect {
            "worker" => "Worker",
            "work" => "Work",
            "project" => "Project",
            "realm" => "Realm",
            "repo" => "Repo",
            _ => "Model",
        }
    }
}

/// Every aspect a session has a value for, in [`ASPECTS`]' order.
pub fn of(s: &Session) -> Vec<Aspect> {
    let live = api::live();
    let mut out = Vec::new();
    let worker = s.worker_id.to_string();
    out.push(Aspect { aspect: "worker", label: live.worker_name(&worker), key: format!("worker:{worker}") });
    if let Some(w) = &s.work {
        out.push(Aspect { aspect: "work", key: format!("work:{}", w.id), label: w.title.clone() });
        if !w.project.is_empty() {
            out.push(Aspect { aspect: "project", key: format!("project:{}", w.project), label: w.project_title.clone() });
        }
    }
    if !s.realm.is_empty() {
        out.push(Aspect { aspect: "realm", key: format!("realm:{}", s.realm), label: s.realm.clone() });
    }
    if let Some(repo) = s.auto_labels.iter().find_map(|l| l.strip_prefix("repo:")) {
        out.push(Aspect { aspect: "repo", key: format!("repo:{repo}"), label: repo.to_owned() });
    }
    if !s.model.is_empty() {
        out.push(Aspect { aspect: "model", key: format!("model:{}", s.model), label: s.model.clone() });
    }
    out
}

/// The ones its realm shows: a realm only when it isn't home (and you
/// aren't on its own page, where every session is in it).
pub fn shown(s: &Session) -> Vec<Aspect> {
    let facets = use_facets();
    let wanted = facets.shown.get();
    let here = api::realm().get();
    of(s)
        .into_iter()
        .filter(|a| wanted.iter().any(|w| w == a.aspect))
        .filter(|a| a.aspect != "realm" || (a.label != "home" && here.is_empty()))
        .collect()
}

/// Which aspects sessions show, kept in the head per realm
/// (`session_facets`).
#[derive(Clone, Copy)]
pub struct Facets {
    pub shown: RwSignal<Vec<String>>,
}

impl Facets {
    pub fn load() -> Self {
        let shown = RwSignal::new(SessionFacets::default().shown);
        spawn_local(async move {
            if let Ok(v) = api::get_setting("session_facets").await
                && let Ok(f) = serde_json::from_value::<SessionFacets>(v)
            {
                shown.set(f.shown);
            }
        });
        Facets { shown }
    }
}

pub fn use_facets() -> Facets {
    expect_context::<Facets>()
}

/// The value you're pointing at: tiles without it dim.
#[derive(Clone, Copy)]
pub struct Spotlight(pub RwSignal<Option<String>>);

pub fn use_spotlight() -> Spotlight {
    expect_context::<Spotlight>()
}

/// A session's shown aspects in a tile's bar: shapes, named when the bar is
/// wide (a container query on the bar).
#[component]
pub fn AspectStrip(#[prop(into)] session: Signal<Option<Session>>) -> impl IntoView {
    let spot = use_spotlight();
    let open = RwSignal::new(None::<(f64, f64)>);
    let picked = RwSignal::new(None::<Aspect>);
    let aspects = Memo::new(move |_| session.get().map(|s| shown(&s)).unwrap_or_default());
    view! {
        <span class="flex min-w-0 shrink items-center gap-0.5">
            <For each=move || aspects.get() key=|a| (a.key.clone(), a.label.clone()) let(a)>
                {
                    let (enter, click) = (a.key.clone(), a.clone());
                    let mark = Signal::derive({ let k = a.key.clone(); move || live_mark(&k) });
                    view! {
                        <button class="flex min-w-0 items-center gap-1 rounded px-1 py-0.5 text-[11px] text-zinc-600 hover:bg-black/10 dark:text-zinc-300 dark:hover:bg-white/10"
                            title=format!("{}: {}", a.what(), a.label)
                            on:mouseenter=move |_| spot.0.set(Some(enter.clone()))
                            on:mouseleave=move |_| spot.0.set(None)
                            on:click=move |ev| { ev.stop_propagation(); spot.0.set(None); picked.set(Some(click.clone())); open.set(Some(below(&ev, 380.0))) }>
                            <Glyph mark shape=a.shape() size=10 />
                            <span class="hidden max-w-28 truncate @2xl:inline">{a.label.clone()}</span>
                        </button>
                    }
                }
            </For>
            <Popover at=open class="w-[380px]">
                {move || picked.get().zip(session.get_untracked()).map(|(a, s)| view! { <AspectCard a session=s on_done=move |()| open.set(None) /> })}
            </Popover>
        </span>
    }
}

/// One value of an aspect: what it is, the other sessions with it, the way
/// to its page, and its palette.
#[component]
fn AspectCard(a: Aspect, session: Session, #[prop(into)] on_done: Callback<()>) -> impl IntoView {
    let live = api::live();
    let key = StoredValue::new(a.key.clone());
    let me = session.id.clone();
    let others = Memo::new(move |_| {
        live.sessions.with(|ss| {
            ss.iter()
                .filter(|s| s.id != me && s.parent_id.is_none() && of(s).iter().any(|x| x.key == key.get_value()))
                .map(|s| (s.id.clone(), s.title.clone(), crate::marks::of(s)))
                .collect::<Vec<_>>()
        })
    });
    let mark = Signal::derive(move || live_mark(&key.get_value()));
    let page = a.view();
    let (sid, title) = (session.id.clone(), session.title.clone());
    let is_work = a.aspect == "work";
    view! {
        <div class="space-y-2 p-1">
            <div class="flex items-center gap-2">
                <Glyph mark shape=a.shape() size=14 />
                <span class="text-[11px] uppercase tracking-wide text-zinc-500">{a.what()}</span>
                <span class="min-w-0 flex-1 truncate font-medium" title=a.label.clone()>{a.label.clone()}</span>
                {page.map(|v| view! { <Go to=v class="shrink-0 text-xs text-blue-600 hover:underline dark:text-blue-400">"Open"</Go> })}
            </div>
            {is_work.then(|| view! { <crate::work::SessionWork session_id=sid title=Signal::stored(title) /> })}
            {move || {
                let list = others.get();
                let n = list.len();
                view! {
                    <div class="space-y-0.5">
                        <div class="text-[11px] text-zinc-500">{match n { 0 => "No other session has it".to_owned(), 1 => "1 other session".to_owned(), n => format!("{n} other sessions") }}</div>
                        {list.into_iter().take(8).map(|(id, title, mk)| view! {
                            <Go to=View::session(id) class="flex items-center gap-1.5 rounded px-1 py-0.5 text-xs hover:bg-zinc-100 dark:hover:bg-zinc-800">
                                <Glyph mark=Signal::stored(mk) shape=Shape::Circle size=8 />
                                <span class="min-w-0 flex-1 truncate">{title}</span>
                            </Go>
                        }).collect_view()}
                        {(n > 8).then(|| view! { <div class="px-1 text-[11px] text-zinc-400">{format!("+{}", n - 8)}</div> })}
                    </div>
                }
            }}
            <div class="border-t border-zinc-200 pt-2 dark:border-zinc-800">
                <div class="pb-1 text-[11px] text-zinc-500">"Its mark, wherever it shows"</div>
                <ValuePalette key=a.key.clone() on_pick=on_done />
            </div>
        </div>
    }
}

/// A session's shown aspects as small shapes, named on hover (rows in the
/// rail and the dock).
#[component]
pub fn AspectGlyphs(s: Session, #[prop(default = 8)] size: u32) -> impl IntoView {
    let session = StoredValue::new(s);
    move || {
        session
            .with_value(shown)
            .into_iter()
            .map(|a| {
                let k = a.key.clone();
                view! {
                    <span class="inline-flex" title=format!("{}: {}", a.what(), a.label)>
                        <Glyph mark=Signal::derive(move || live_mark(&k)) shape=a.shape() size />
                    </span>
                }
            })
            .collect_view()
    }
}

/// A session's shown aspects as named chips (the sessions list, cards).
#[component]
pub fn AspectChips(s: Session) -> impl IntoView {
    let session = StoredValue::new(s);
    move || {
        let list = session.with_value(shown);
        (!list.is_empty()).then(|| {
            view! {
                <span class="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-0.5">
                    {list.into_iter().map(|a| {
                        let k = a.key.clone();
                        view! {
                            <span class="inline-flex min-w-0 items-center gap-1 text-xs text-zinc-500" title=a.what()>
                                <Glyph mark=Signal::derive(move || live_mark(&k)) shape=a.shape() size=9 />
                                <span class="max-w-40 truncate">{a.label.clone()}</span>
                            </span>
                        }
                    }).collect_view()}
                </span>
            }
        })
    }
}

/// Settings → Appearance: which aspects sessions show.
#[component]
pub fn FacetsSetting() -> impl IntoView {
    let facets = use_facets();
    let error = RwSignal::new(None::<String>);
    let toggle = move |aspect: &'static str, on: bool| {
        let mut shown = facets.shown.get_untracked();
        shown.retain(|a| a != aspect);
        if on {
            shown.push(aspect.to_owned());
        }
        shown.sort_by_key(|a| ASPECTS.iter().position(|x| x == a));
        facets.shown.set(shown.clone());
        spawn_local(async move { error.set(api::put_setting("session_facets", serde_json::json!({ "shown": shown })).await.err()) });
    };
    let what = |aspect: &str| match aspect {
        "worker" => ("Worker", "The machine it runs on"),
        "work" => ("Work", "The work it serves; its tile gets a stripe in the work's mark too"),
        "project" => ("Project", "The project its work is in"),
        "realm" => ("Realm", "When it isn't home"),
        "repo" => ("Repo", "The git repository it works in"),
        _ => ("Model", "The model it uses"),
    };
    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <h2 class="font-medium">"Session aspects"</h2>
            <p class="text-xs text-zinc-500">
                "What a session's tile bar, its row and its dock line show, each in its own shape. Every value wears the same mark everywhere: click one in a tile's bar to change it, or point at it to dim the tiles without it."
            </p>
            <div class="grid gap-2 sm:grid-cols-2">
                {ASPECTS.into_iter().map(|aspect| {
                    let (label, hint) = what(aspect);
                    view! {
                        <label class="flex cursor-pointer items-start gap-3 rounded-md border border-zinc-200 px-3 py-2 has-checked:border-blue-500 has-checked:bg-blue-50 dark:border-zinc-800 dark:has-checked:border-blue-400 dark:has-checked:bg-blue-950">
                            <input type="checkbox" class="mt-1" prop:checked=move || facets.shown.with(|s| s.iter().any(|a| a == aspect))
                                on:change=move |ev| toggle(aspect, event_target_checked(&ev)) />
                            <span class="mt-1"><Glyph mark=Signal::stored("slate".to_owned()) shape=Shape::of(aspect) size=11 /></span>
                            <span>
                                <span class="block text-sm font-medium">{label}</span>
                                <span class="block text-xs text-zinc-500">{hint}</span>
                            </span>
                        </label>
                    }
                }).collect_view()}
            </div>
            <p class="text-xs text-zinc-500">"Kept in carbon6, for this realm."</p>
            {move || error.get().map(|e| view! { <p class="text-xs text-red-600">{e}</p> })}
        </section>
    }
}
