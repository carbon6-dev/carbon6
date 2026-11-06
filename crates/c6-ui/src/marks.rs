//! Marks (docs/marks.md): a colour, plain or in a pattern. Each session has
//! one (picked when it starts, changed by you): its tile's bar, its row in
//! the rail and its line in the dock wear it, so you can tell which tile is
//! which at a glance. Each value of a session's aspects (its worker, its
//! work, its project, its realm, its repo, its model) has one too, worn in the aspect's
//! own shape wherever that value shows.

use c6_api::Session;
use c6_api::marks::{self as m, Marks};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::api;

/// Each colour's swatch: bright enough to read on white and on the dark
/// theme's near-black, far enough apart to tell at a glance.
pub fn hex(colour: &str) -> &'static str {
    match colour {
        "sky" => "#0ea5e9",
        "emerald" => "#10b981",
        "amber" => "#f59e0b",
        "rose" => "#f43f5e",
        "violet" => "#8b5cf6",
        "teal" => "#14b8a6",
        "orange" => "#f97316",
        "indigo" => "#6366f1",
        "lime" => "#84cc16",
        "pink" => "#ec4899",
        "cyan" => "#06b6d4",
        "fuchsia" => "#d946ef",
        "red" => "#dc2626",
        "yellow" => "#facc15",
        "blue" => "#2563eb",
        "brown" => "#92400e",
        "navy" => "#1e3a8a",
        "olive" => "#4d7c0f",
        "maroon" => "#881337",
        "slate" => "#64748b",
        _ => "#a1a1aa",
    }
}

/// A mark's colour alone (for a border or a stripe).
pub fn colour_hex(mark: &str) -> &'static str {
    hex(m::parse(mark).map(|(c, _)| c).unwrap_or(""))
}

/// A mark as CSS for a swatch's background: the colour, or its pattern on
/// a pale tint of it.
pub fn fill(mark: &str) -> String {
    let (colour, pattern) = m::parse(mark).unwrap_or(("", ""));
    let c = hex(colour);
    let tint = format!("color-mix(in srgb,{c} 28%,white)");
    match pattern {
        "hstripes" => format!("background:repeating-linear-gradient(0deg,{c} 0 2px,{tint} 2px 4px)"),
        "vstripes" => format!("background:repeating-linear-gradient(90deg,{c} 0 2px,{tint} 2px 4px)"),
        "diagonal" => format!("background:repeating-linear-gradient(45deg,{c} 0 2px,{tint} 2px 4px)"),
        "dots" => format!("background:radial-gradient(circle,{c} 1.3px,transparent 1.7px) 0 0/4px 4px,{tint}"),
        _ => format!("background:{c}"),
    }
}

/// A session's mark: the one it has, or (sessions from before colours) one
/// from its id.
pub fn of(s: &Session) -> String {
    if m::is_mark(&s.colour) { s.colour.clone() } else { m::hashed(&s.id).to_owned() }
}

/// The mark of a session by id, from the live list or its open tile's metadata.
pub fn of_id(id: &str) -> Option<String> {
    api::live().session(id).as_ref().map(of)
}

/// The mark of an aspect's value (`worker:<id>`, `realm:home`…): the one
/// given it, else one worked out from it (realms keep the colours they
/// always had).
pub fn mark_of(marks: &Marks, key: &str) -> String {
    if let Some(given) = marks.get(key).filter(|g| m::is_mark(g)) {
        return given.clone();
    }
    let value = key.split_once(':').map_or(key, |(_, v)| v);
    match key {
        "realm:home" => "sky".into(),
        "realm:commons" => "amber".into(),
        _ if key.starts_with("realm:") => {
            const OTHERS: [&str; 6] = ["emerald", "violet", "rose", "teal", "indigo", "lime"];
            OTHERS[value.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619)) as usize % OTHERS.len()].into()
        }
        _ => m::hashed(value).into(),
    }
}

/// The mark of a value, from the live marks.
pub fn live_mark(key: &str) -> String {
    api::live().marks.with(|ms| mark_of(ms, key))
}

/// The shape each aspect's marks come in: a session's own is a circle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Circle,
    Square,
    Diamond,
    Triangle,
    Hexagon,
    Pentagon,
    Pill,
}

impl Shape {
    pub fn of(aspect: &str) -> Shape {
        match aspect {
            "worker" => Shape::Square,
            "work" => Shape::Diamond,
            "project" => Shape::Hexagon,
            "realm" => Shape::Triangle,
            "repo" => Shape::Pentagon,
            "model" => Shape::Pill,
            _ => Shape::Circle,
        }
    }

    /// Its size and outline as CSS, for a glyph `px` high.
    pub fn css(self, px: u32) -> String {
        let (w, clip, radius) = match self {
            Shape::Circle => (px, "", "9999px"),
            Shape::Square => (px, "", "2px"),
            Shape::Diamond => (px + 2, "polygon(50% 0,100% 50%,50% 100%,0 50%)", "0"),
            Shape::Triangle => (px + 1, "polygon(50% 0,100% 100%,0 100%)", "0"),
            Shape::Hexagon => (px + 1, "polygon(25% 4%,75% 4%,100% 50%,75% 96%,25% 96%,0 50%)", "0"),
            Shape::Pentagon => (px + 1, "polygon(50% 0,100% 38%,81% 100%,19% 100%,0 38%)", "0"),
            Shape::Pill => (px * 8 / 5, "", "9999px"),
        };
        let h = if matches!(self, Shape::Diamond | Shape::Triangle | Shape::Hexagon | Shape::Pentagon) { px + 1 } else { px };
        let clip = if clip.is_empty() { String::new() } else { format!(";clip-path:{clip}") };
        format!("width:{w}px;height:{h}px;border-radius:{radius}{clip}")
    }
}

/// A mark in a shape, outlined so pale ones show on white and dark ones on
/// the dark theme.
#[component]
pub fn Glyph(#[prop(into)] mark: Signal<String>, shape: Shape, #[prop(default = 12)] size: u32) -> impl IntoView {
    view! {
        <span class="inline-flex shrink-0 drop-shadow-[0_0_0.6px_rgb(0_0_0/0.5)] dark:drop-shadow-[0_0_0.7px_rgb(255_255_255/0.55)]">
            <span class="block" style=move || format!("{};{}", shape.css(size), fill(&mark.get()))></span>
        </span>
    }
}

/// A value's mark (by key, `work:<id>`) in its aspect's shape.
#[component]
pub fn ValueGlyph(#[prop(into)] key: String, #[prop(default = 10)] size: u32) -> impl IntoView {
    let shape = Shape::of(key.split_once(':').map_or("", |(a, _)| a));
    view! { <Glyph mark=Signal::derive(move || live_mark(&key)) shape size /> }
}

/// A value's mark as a button: click to change it.
#[component]
pub fn MarkButton(#[prop(into)] key: String, shape: Shape, #[prop(default = 12)] size: u32) -> impl IntoView {
    let at = RwSignal::new(None::<(f64, f64)>);
    let k = key.clone();
    view! {
        <button class="flex shrink-0 hover:scale-125" title="Its mark, the same everywhere it shows: change it"
            on:click=move |ev| { ev.stop_propagation(); ev.prevent_default(); at.set(Some(crate::workspace::below(&ev, 380.0))) }>
            <Glyph mark=Signal::derive(move || live_mark(&k)) shape size />
        </button>
        <crate::workspace::Popover at class="w-auto">
            <ValuePalette key=key.clone() on_pick=move |()| at.set(None) />
        </crate::workspace::Popover>
    }
}

/// Every mark to pick from: a row of plain colours, then a row per
/// pattern; the current one ringed. `clear`: a way back to the one worked
/// out from the value.
#[component]
pub fn Palette(
    #[prop(into)] current: Signal<Option<String>>,
    on_pick: Callback<String>,
    #[prop(default = None)] clear: Option<Callback<()>>,
) -> impl IntoView {
    let rows = std::iter::once("").chain(m::PATTERNS).map(|p| {
        let marks: Vec<String> = m::COLOURS.into_iter().map(|c| if p.is_empty() { c.to_owned() } else { format!("{c}/{p}") }).collect();
        view! {
            <div class="flex flex-wrap gap-[3px]">
                {marks.into_iter().map(|mk| {
                    let (a, b, c) = (mk.clone(), mk.clone(), mk.clone());
                    view! {
                        <button
                            class=move || format!("h-3.5 w-3.5 shrink-0 rounded-[3px] ring-1 ring-black/10 dark:ring-white/15 {}",
                                if current.get().as_deref() == Some(a.as_str()) { "ring-2 ring-zinc-900! ring-offset-1 dark:ring-zinc-100! dark:ring-offset-zinc-900" } else { "hover:scale-125" })
                            style=fill(&b)
                            title=b.replace('/', " · ")
                            on:click=move |ev| { ev.stop_propagation(); on_pick.run(c.clone()) }
                        ></button>
                    }
                }).collect_view()}
            </div>
        }
    });
    view! {
        <div class="space-y-[3px]">
            {rows.collect_view()}
            {clear.map(|f| view! {
                <button class="mt-1 text-[11px] text-muted hover:underline" on:click=move |ev| { ev.stop_propagation(); f.run(()) }>
                    "Back to its own colour"
                </button>
            })}
        </div>
    }
}

/// The palette for a session's own mark.
#[component]
pub fn SessionPalette(session: String, #[prop(optional, into)] on_pick: Option<Callback<()>>) -> impl IntoView {
    let id = StoredValue::new(session);
    let current = Signal::derive(move || of_id(&id.get_value()));
    let error = RwSignal::new(None::<String>);
    let pick = Callback::new(move |mark: String| {
        spawn_local(async move { error.set(api::set_colour(id.get_value(), mark).await.err()) });
        if let Some(f) = on_pick {
            f.run(())
        }
    });
    view! {
        <div class="space-y-1">
            <Palette current on_pick=pick />
            {move || error.get().map(|e| view! { <p class="text-[11px] text-red-600">{e}</p> })}
        </div>
    }
}

/// A looking card starts with just its current mark, not a hundred swatches.
/// Keep the picker in the card's flow so it remains part of the hover target.
#[component]
pub fn SessionSwatch(session: Session) -> impl IntoView {
    let open = RwSignal::new(false);
    let id = StoredValue::new(session.id.clone());
    let fallback = of(&session);
    let current = Signal::derive(move || of_id(&id.get_value()).unwrap_or_else(|| fallback.clone()));
    view! {
        <div class="space-y-2">
            <button class=crate::icons::BAR_BUTTON title="This session's mark: change it" aria-label="Change session mark"
                aria-expanded=move || open.get().to_string()
                on:click=move |ev| { ev.stop_propagation(); open.update(|o| *o = !*o) }>
                <Glyph mark=current shape=Shape::Circle size=16 />
            </button>
            <Show when=move || open.get()>
                <SessionPalette session=id.get_value() />
            </Show>
        </div>
    }
}

/// The palette for an aspect's value (`worker:<id>`, `work:<id>`…).
#[component]
pub fn ValuePalette(#[prop(into)] key: String, #[prop(optional, into)] on_pick: Option<Callback<()>>) -> impl IntoView {
    let key = StoredValue::new(key);
    let live = api::live();
    let current =
        Signal::derive(move || live.marks.with(|ms| ms.get(&key.get_value()).cloned()).or_else(|| Some(live_mark(&key.get_value()))));
    let error = RwSignal::new(None::<String>);
    let set = move |mark: String| {
        spawn_local(async move { error.set(api::set_mark(key.get_value(), mark).await.err()) });
        if let Some(f) = on_pick {
            f.run(())
        }
    };
    // Workers, work and projects get one picked again when theirs is taken
    // away, so only the others go back to the one worked out from their name.
    let picked_for_it = key.with_value(|k| ["worker:", "work:", "project:"].iter().any(|p| k.starts_with(p)));
    let given = move || !picked_for_it && live.marks.with(|ms| ms.contains_key(&key.get_value()));
    view! {
        <div class="space-y-1">
            {move || {
                let clear = given().then(|| Callback::new(move |()| set(String::new())));
                view! { <Palette current on_pick=Callback::new(set) clear /> }
            }}
            {move || error.get().map(|e| view! { <p class="text-[11px] text-red-600">{e}</p> })}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_session_card_starts_with_just_its_current_swatch() {
        let owner = Owner::new();
        owner.with(|| {
            provide_context(api::LiveFeed {
                workers: signal(Vec::new()).0,
                sessions: signal(Vec::new()).0,
                resolved_sessions: RwSignal::new(Default::default()),
                connected: signal(false).0,
                doc: signal(None).0,
                marks: signal(Default::default()).0,
                attention: signal(0).0,
            });
            for mark in ["navy/dots", "rose/diagonal", "sky", ""] {
                let session: Session = serde_json::from_value(serde_json::json!({
                    "id": "older-session", "worker_id": "00000000-0000-0000-0000-000000000001",
                    "title": "A session", "cwd": "", "connection_id": "c", "model": "m", "status": "idle",
                    "error": null, "created_at": 0, "updated_at": 0, "last_seq": 0, "colour": mark
                }))
                .unwrap();
                let expected = fill(&of(&session));
                let html = view! { <SessionSwatch session /> }.to_html();
                assert_eq!(html.matches("<button").count(), 1, "collapsed: {html}");
                assert!(html.contains("aria-expanded=\"false\""), "{html}");
                assert!(html.contains(&expected), "keep the pattern or id fallback: {html}");
            }
        });
    }

    #[test]
    fn every_mark_fills_in_its_own_colour() {
        for mk in m::all() {
            let (c, _) = m::parse(&mk).unwrap();
            assert_ne!(hex(c), hex("none"), "{mk}");
            assert!(fill(&mk).contains(hex(c)), "{mk}");
        }
        let hexes: std::collections::HashSet<_> = m::COLOURS.into_iter().map(hex).collect();
        assert_eq!(hexes.len(), m::COLOURS.len(), "no two colours alike");
    }

    #[test]
    fn values_without_a_mark_get_one_from_their_name() {
        let mut given = Marks::new();
        assert_eq!(mark_of(&given, "realm:home"), "sky");
        assert_eq!(mark_of(&given, "realm:commons"), "amber");
        assert_eq!(mark_of(&given, "repo:carbon6"), mark_of(&given, "repo:carbon6"));
        given.insert("realm:home".into(), "navy/dots".into());
        assert_eq!(mark_of(&given, "realm:home"), "navy/dots");
    }
}
