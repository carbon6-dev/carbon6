//! A session's aspects (docs/marks.md): its worker, its work, its project,
//! its realm, its repo and its model, each value in its mark and its aspect's shape.
//! Rows and the dock show the ones its realm chose (Settings → Appearance).
//! Desktop tile bars opt in separately; their entire controller is unmounted
//! when off or on a phone, with no per-tile reactive work on the default path.

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
    // A session the head runs has no worker, so no worker chip.
    if !s.on_head {
        let worker = s.worker_id.to_string();
        out.push(Aspect { aspect: "worker", label: live.worker_name(&worker), key: format!("worker:{worker}") });
    }
    if let Some(w) = &s.work {
        // A quick session (docs/projects.md §3.1) has a project and no work.
        if !w.id.is_empty() {
            out.push(Aspect { aspect: "work", key: format!("work:{}", w.id), label: w.title.clone() });
        }
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
    pub tile_bars: RwSignal<bool>,
}

impl Facets {
    pub fn load() -> Self {
        let shown = RwSignal::new(SessionFacets::default().shown);
        let tile_bars = RwSignal::new(false);
        spawn_local(async move {
            if let Ok(v) = api::get_setting("session_facets").await
                && let Ok(f) = serde_json::from_value::<SessionFacets>(v)
            {
                shown.set(f.shown);
                tile_bars.set(f.tile_bars);
            }
        });
        Facets { shown, tile_bars }
    }

    fn save(self, error: RwSignal<Option<String>>) {
        let value = SessionFacets { shown: self.shown.get_untracked(), tile_bars: self.tile_bars.get_untracked() };
        spawn_local(async move { error.set(api::put_setting("session_facets", serde_json::json!(value)).await.err()) });
    }
}

pub fn use_facets() -> Facets {
    expect_context::<Facets>()
}

pub fn tile_bars_shown(enabled: bool, phone: bool) -> bool {
    enabled && !phone
}

/// Mounted once by Tiles, only while opted in on desktop. Portals fill inert
/// slots in the existing bars: toggling never remounts a tile or its controls.
/// Off has no per-tile signals, listeners, timers or session/aspect scans.
#[component]
pub fn TileMarks(keys: Memo<Vec<u64>>) -> impl IntoView {
    let mounts = RwSignal::new(Vec::<u64>::new());
    let spot = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        let keys = keys.get();
        // New bars must be mounted before looking up their static slots.
        request_animation_frame(move || {
            mounts.try_set(keys);
        });
    });
    view! {
        <For each=move || mounts.get() key=|key| *key let:key>
            <TileAspects key spot />
        </For>
    }
}

#[component]
fn TileAspects(key: u64, spot: RwSignal<Option<String>>) -> impl IntoView {
    let ws = crate::workspace::use_workspace();
    let live = api::live();
    let session = Memo::new(move |_| {
        ws.layout.with(|l| {
            let col = l.bar_column(key)?;
            let c = l.tile(key)?.columns.iter().find(|c| c.key == col)?;
            match &c.view {
                View::Session { id, .. } => live.session(id),
                _ => None,
            }
        })
    });
    let slot = document().query_selector(&format!("[data-tile-aspects='{key}']")).ok().flatten();
    let panel = slot.as_ref().and_then(|slot| slot.closest("section").ok().flatten()).map(send_wrapper::SendWrapper::new);
    if let Some(panel) = panel {
        Effect::new({
            let panel = panel.clone();
            move |_| {
                let dim = spot.get().is_some_and(|k| !session.get().is_some_and(|s| of(&s).iter().any(|a| a.key == k)));
                let _ = panel.set_attribute("data-aspect-dim", if dim { "true" } else { "false" });
            }
        });
        on_cleanup(move || {
            let _ = panel.remove_attribute("data-aspect-dim");
        });
    }
    slot.map(|mount| {
        view! {
            <leptos::portal::Portal mount>
                <AspectStrip session spot />
            </leptos::portal::Portal>
        }
    })
}

/// A resting pointer, not one passing over a bar, spotlights a value.
const SPOTLIGHT_DWELL_MS: u64 = 400;

#[component]
fn AspectStrip(session: Memo<Option<Session>>, spot: RwSignal<Option<String>>) -> impl IntoView {
    let timer = StoredValue::new(None::<TimeoutHandle>);
    let hovered = StoredValue::new(None::<String>);
    let rest_on = move |key: Option<String>| {
        if let Some(t) = timer.get_value() {
            t.clear();
        }
        timer.set_value(None);
        hovered.set_value(key.clone());
        spot.set(None);
        if let Some(k) = key {
            timer.set_value(set_timeout_with_handle(move || spot.set(Some(k)), std::time::Duration::from_millis(SPOTLIGHT_DWELL_MS)).ok());
        }
    };
    on_cleanup(move || {
        if let Some(t) = timer.try_get_value().flatten() {
            t.clear();
        }
        // A tile/aspect can disappear without mouseleave (closing a tile,
        // changing the shown facets, resizing to phone, or switching off).
        if let Some(Some(key)) = hovered.try_get_value()
            && spot.try_get_untracked().flatten().as_ref() == Some(&key)
        {
            spot.try_set(None);
        }
    });
    let open = RwSignal::new(None::<(f64, f64)>);
    let picked = RwSignal::new(None::<Aspect>);
    let aspects = Memo::new(move |_| session.get().map(|s| shown(&s)).unwrap_or_default());
    Effect::new(move |_| {
        aspects.track();
        if hovered.get_value().is_some() {
            rest_on(None);
        }
    });
    view! {
        <span class="mx-2 flex items-center gap-2" data-aspect-strip>
            <For each=move || aspects.get() key=|a| (a.key.clone(), a.label.clone()) let(a)>
                {
                    let (enter, click) = (a.key.clone(), a.clone());
                    let mark = Signal::derive({ let k = a.key.clone(); move || live_mark(&k) });
                    view! {
                        <button class="flex h-11 w-4 shrink-0 items-center justify-center rounded hover:bg-black/10 dark:hover:bg-white/10"
                            title=format!("{}: {}", a.what(), a.label)
                            aria-label=format!("{}: {}", a.what(), a.label)
                            on:mouseenter=move |_| rest_on(Some(enter.clone()))
                            on:mouseleave=move |_| rest_on(None)
                            on:click=move |ev| { ev.stop_propagation(); rest_on(None); picked.set(Some(click.clone())); open.set(Some(below(&ev, 380.0))) }>
                            <Glyph mark shape=a.shape() size=10 />
                        </button>
                    }
                }
            </For>
            <leptos::portal::Portal>
                <Popover at=open class="w-[380px]">
                    {move || picked.get().zip(session.get_untracked()).map(|(a, s)| view! { <AspectCard a session=s on_done=move |()| open.set(None) /> })}
                </Popover>
            </leptos::portal::Portal>
        </span>
    }
}

/// A value's related sessions, destination and shared palette.
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
                <span class="text-[11px] uppercase tracking-wide text-zinc-500 dark:text-zinc-400">{a.what()}</span>
                <span class="min-w-0 flex-1 truncate font-medium" title=a.label.clone()>{a.label.clone()}</span>
                {page.map(|v| view! { <Go to=v class="inline-flex shrink-0 items-center gap-1 text-xs text-blue-600 hover:underline dark:text-blue-400">"Open"{crate::icons::icon(crate::icons::Icon::ArrowUpRight, crate::icons::Size::S14)}</Go> })}
            </div>
            {is_work.then(|| view! { <crate::work::SessionWork session_id=sid title=Signal::stored(title) /> })}
            {move || {
                let list = others.get();
                let n = list.len();
                view! {
                    <div class="space-y-0.5">
                        <div class="text-[11px] text-zinc-500 dark:text-zinc-400">{match n { 0 => "No other session has it".to_owned(), 1 => "1 other session".to_owned(), n => format!("{n} other sessions") }}</div>
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
                <div class="pb-1 text-[11px] text-zinc-500 dark:text-zinc-400">"Its mark, wherever it shows"</div>
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
                            <span class="inline-flex min-w-0 items-center gap-1 text-xs text-muted" title=a.what()>
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
        facets.shown.set(shown);
        facets.save(error);
    };
    let what = |aspect: &str| match aspect {
        "worker" => ("Worker", "The machine it runs on"),
        "work" => ("Work", "The work it serves"),
        "project" => ("Project", "The project its work is in"),
        "realm" => ("Realm", "When it isn't home"),
        "repo" => ("Repo", "The git repository it works in"),
        _ => ("Model", "The model it uses"),
    };
    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <h2 class="font-medium">"Session aspects"</h2>
            <p class="text-xs text-muted">
                "What a session's row and its dock line show, each in its own shape. Every value wears the same mark everywhere."
            </p>
            <div class="grid gap-2 @lg/settings:grid-cols-2">
                {ASPECTS.into_iter().map(|aspect| {
                    let (label, hint) = what(aspect);
                    view! {
                        <div class=move || format!("flex items-center gap-3 rounded-md border px-3 py-2 {}", if facets.shown.with(|s| s.iter().any(|a| a == aspect)) {
                            "border-blue-500 bg-blue-50 dark:border-blue-400 dark:bg-blue-950"
                        } else {
                            "border-zinc-200 dark:border-zinc-800"
                        })>
                            <span><Glyph mark=Signal::stored("slate".to_owned()) shape=Shape::of(aspect) size=11 /></span>
                            <crate::components::Switch on=move || facets.shown.with(|s| s.iter().any(|a| a == aspect))
                                on_change=move |on| toggle(aspect, on) label hint />
                        </div>
                    }
                }).collect_view()}
            </div>
            <crate::components::Switch on=facets.tile_bars label="Show on tile bars"
                hint="Desktop only: show these shapes after the title; rest on a value to dim other tiles, or click for its card."
                on_change=move |on| { facets.tile_bars.set(on); facets.save(error); } />
            <p class="text-xs text-muted">"Kept in carbon6, for this realm."</p>
            {move || error.get().map(|e| view! { <p class="text-xs text-red-600">{e}</p> })}
        </section>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_marks_show_only_when_opted_in_on_desktop() {
        assert!(!tile_bars_shown(false, false));
        assert!(!tile_bars_shown(false, true));
        assert!(!tile_bars_shown(true, true));
        assert!(tile_bars_shown(true, false));
    }

    #[test]
    fn default_tiles_have_only_static_aspect_slots() {
        let source = include_str!("workspace.rs");
        let tile = source.split("fn TileView(").nth(1).unwrap().split("#[cfg(test)]").next().unwrap();
        assert!(tile.contains("data-tile-aspects=key"));
        for enabled_only in ["AspectStrip", "TileAspects", "use_facets", "spotlight", "aspects::of", "aspects::shown"] {
            assert!(!tile.contains(enabled_only), "{enabled_only} must stay behind the one workspace-level gate");
        }
    }
}
