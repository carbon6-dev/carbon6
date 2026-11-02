//! A session's aspects (docs/marks.md): its worker, its work, its project,
//! its realm, its repo and its model, each value in its mark and its aspect's shape.
//! A session's row and its dock line show the ones its realm chose
//! (Settings → Appearance).

use c6_api::Session;
use c6_api::marks::{ASPECTS, SessionFacets};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;
use crate::marks::{Glyph, Shape, live_mark};

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
        facets.shown.set(shown.clone());
        spawn_local(async move { error.set(api::put_setting("session_facets", serde_json::json!({ "shown": shown })).await.err()) });
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
                                <span class="block text-xs text-muted">{hint}</span>
                            </span>
                        </label>
                    }
                }).collect_view()}
            </div>
            <p class="text-xs text-muted">"Kept in carbon6, for this realm."</p>
            {move || error.get().map(|e| view! { <p class="text-xs text-red-600">{e}</p> })}
        </section>
    }
}
