//! Session colours: each session has one (picked when it starts, changed
//! by you), and its tile's bar, its row in the rail and its dot in the dock
//! wear it, so you can tell which tile is which at a glance.

use c6_api::{SESSION_COLOURS, Session};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;

/// A session's colour: the one it has, or (sessions from before colours)
/// one from its id.
pub fn of(s: &Session) -> &'static str {
    SESSION_COLOURS.into_iter().find(|c| *c == s.colour).unwrap_or_else(|| by_id(&s.id))
}

/// The colour of a session by id, from the live list.
pub fn of_id(id: &str) -> Option<&'static str> {
    api::live().sessions.with(|ss| ss.iter().find(|s| s.id == id).map(of))
}

fn by_id(id: &str) -> &'static str {
    let h = id.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    SESSION_COLOURS[h as usize % SESSION_COLOURS.len()]
}

/// A tile bar's background and border in this colour: a tint, so the
/// text on it reads in light and dark.
// Whole class names, so Tailwind finds them in the source.
pub fn bar(c: &str) -> &'static str {
    match c {
        "sky" => "bg-sky-100 border-sky-200 dark:bg-sky-950 dark:border-sky-900",
        "emerald" => "bg-emerald-100 border-emerald-200 dark:bg-emerald-950 dark:border-emerald-900",
        "amber" => "bg-amber-100 border-amber-200 dark:bg-amber-950 dark:border-amber-900",
        "rose" => "bg-rose-100 border-rose-200 dark:bg-rose-950 dark:border-rose-900",
        "violet" => "bg-violet-100 border-violet-200 dark:bg-violet-950 dark:border-violet-900",
        "teal" => "bg-teal-100 border-teal-200 dark:bg-teal-950 dark:border-teal-900",
        "orange" => "bg-orange-100 border-orange-200 dark:bg-orange-950 dark:border-orange-900",
        "indigo" => "bg-indigo-100 border-indigo-200 dark:bg-indigo-950 dark:border-indigo-900",
        "lime" => "bg-lime-100 border-lime-200 dark:bg-lime-950 dark:border-lime-900",
        "pink" => "bg-pink-100 border-pink-200 dark:bg-pink-950 dark:border-pink-900",
        "cyan" => "bg-cyan-100 border-cyan-200 dark:bg-cyan-950 dark:border-cyan-900",
        "fuchsia" => "bg-fuchsia-100 border-fuchsia-200 dark:bg-fuchsia-950 dark:border-fuchsia-900",
        _ => "bg-zinc-50 border-zinc-200 dark:bg-zinc-900 dark:border-zinc-800",
    }
}

/// A solid swatch or dot in this colour.
pub fn solid(c: &str) -> &'static str {
    match c {
        "sky" => "bg-sky-500",
        "emerald" => "bg-emerald-500",
        "amber" => "bg-amber-500",
        "rose" => "bg-rose-500",
        "violet" => "bg-violet-500",
        "teal" => "bg-teal-500",
        "orange" => "bg-orange-500",
        "indigo" => "bg-indigo-500",
        "lime" => "bg-lime-500",
        "pink" => "bg-pink-500",
        "cyan" => "bg-cyan-500",
        "fuchsia" => "bg-fuchsia-500",
        _ => "bg-zinc-400",
    }
}

/// The palette, to change a session's colour: the current one ringed.
#[component]
pub fn Palette(session: String, #[prop(optional, into)] on_pick: Option<Callback<()>>) -> impl IntoView {
    let id = StoredValue::new(session);
    let current = Memo::new(move |_| of_id(&id.get_value()));
    let error = RwSignal::new(None::<String>);
    view! {
        <div class="space-y-1">
            <div class="grid grid-cols-6 gap-1.5">
                {SESSION_COLOURS.into_iter().map(|c| view! {
                    <button
                        class=move || format!("h-5 w-5 rounded-full {} {}", solid(c),
                            if current.get() == Some(c) { "ring-2 ring-zinc-900 ring-offset-1 dark:ring-zinc-100 dark:ring-offset-zinc-900" } else { "hover:scale-110" })
                        title=c
                        on:click=move |ev| {
                            ev.stop_propagation();
                            spawn_local(async move { error.set(api::set_colour(id.get_value(), c.to_owned()).await.err()) });
                            if let Some(f) = on_pick { f.run(()) }
                        }
                    ></button>
                }).collect_view()}
            </div>
            {move || error.get().map(|e| view! { <p class="text-[11px] text-red-600">{e}</p> })}
        </div>
    }
}
