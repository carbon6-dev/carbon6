//! Shared chrome. Immediate changes use Switch; submitted choices and lists
//! keep a native checkbox with CHECKBOX. Button/pill/overlay call-site migrations
//! are deliberately separate from these foundations.

use crate::icons::{Icon, Size, icon};
use leptos::prelude::*;

pub const CHECKBOX: &str = "size-4 shrink-0 accent-sky-600 focus-visible:outline-2 focus-visible:outline-sky-500/60";
pub const SECTION_LABEL: &str = "text-[11px] font-medium uppercase tracking-wide text-muted";

// `.btn` owns default sizing in the components layer, so BTN_SM's utilities
// override it without depending on class-string order. Touch still wins.
pub const BTN_PRIMARY: &str = "btn bg-zinc-900 text-white hover:bg-zinc-700 dark:bg-zinc-100 dark:text-zinc-900 dark:hover:bg-zinc-300";
pub const BTN_SECONDARY: &str =
    "btn border border-zinc-300 bg-white hover:bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-900 dark:hover:bg-zinc-800";
pub const BTN_QUIET: &str = "btn hover:bg-zinc-100 dark:hover:bg-zinc-800";
pub const BTN_DANGER: &str = "btn text-red-600 hover:bg-red-50 dark:text-red-400 dark:hover:bg-red-950";
pub const BTN_DANGER_SOLID: &str = "btn bg-red-600 text-white hover:bg-red-700";
pub const BTN_MD: &str = "min-h-8 px-3 text-sm";
pub const BTN_SM: &str = "min-h-7 px-2.5 text-xs";

pub const BAR_BUTTON: &str = "flex size-7 shrink-0 items-center justify-center rounded text-zinc-600 outline-none focus-visible:ring-2 focus-visible:ring-sky-500/60 disabled:opacity-30 pointer-fine:hover:bg-black/10 pointer-fine:hover:text-zinc-900 pointer-fine:disabled:hover:bg-transparent pointer-coarse:size-10 dark:text-zinc-400 dark:pointer-fine:hover:bg-white/10 dark:pointer-fine:hover:text-zinc-100";
/// Phone/coarse targets stay 44px; a desktop mouse only needs a 28px slot.
pub const SESSION_BAR_BUTTON: &str = "flex size-11 shrink-0 items-center justify-center gap-0.5 rounded-md text-soft outline-none hover:bg-zinc-200/70 focus-visible:ring-2 focus-visible:ring-sky-500/60 disabled:opacity-40 md:pointer-fine:size-7 dark:hover:bg-zinc-800";

pub const INPUT: &str = "w-full rounded-md border border-zinc-300 bg-white px-2 py-1.5 text-sm outline-none focus:border-sky-500 focus:ring-2 focus:ring-sky-500/30 pointer-coarse:min-h-10 pointer-coarse:text-base dark:border-zinc-700 dark:bg-zinc-900";
// The background is the same Lucide chevron-down, encoded as a CSS image.
// A real select retains its native keyboard behaviour and accessible label.
pub const SELECT: &str = "c6-select rounded-md border border-zinc-300 bg-white py-1.5 pl-2 pr-7 text-sm outline-none focus:border-sky-500 focus:ring-2 focus:ring-sky-500/30 pointer-coarse:min-h-10 pointer-coarse:text-base dark:border-zinc-700 dark:bg-zinc-900";

/// The entire row toggles; the track always trails the label/description.
/// Space and Enter work natively. Disabled rows cannot invoke persistence.
#[component]
pub fn Switch(
    #[prop(into)] on: Signal<bool>,
    on_change: impl Fn(bool) + Clone + 'static,
    #[prop(into)] label: String,
    #[prop(optional, into)] title: String,
    #[prop(optional, into)] hint: String,
    #[prop(optional, into)] disabled: Signal<bool>,
) -> impl IntoView {
    switch_row(on, UnsyncCallback::new(on_change), label, title, hint, disabled)
}

// Keep the shared row's view/effects out of every caller's generic handler type.
fn switch_row(
    on: Signal<bool>,
    on_change: UnsyncCallback<bool>,
    label: String,
    title: String,
    hint: String,
    disabled: Signal<bool>,
) -> impl IntoView {
    let accessible_label = label.clone();
    view! {
        <button type="button" role="switch" aria-checked=move || on.get().to_string()
            aria-label=accessible_label title=title disabled=move || disabled.get()
            class="flex min-h-8 w-full items-center justify-between gap-3 rounded px-1 text-left text-sm outline-none focus-visible:ring-2 focus-visible:ring-sky-500/60 disabled:opacity-50 pointer-coarse:min-h-11"
            on:click=move |_| if !disabled.get_untracked() { on_change.run(!on.get_untracked()) }>
            <span class="min-w-0"><span>{label}</span>
                {(!hint.is_empty()).then(|| view! { <span class="block text-xs text-muted">{hint}</span> })}
            </span>
            <span aria-hidden="true" class=move || format!("relative h-4 w-7 shrink-0 rounded-full transition-colors {}", if on.get() { "bg-sky-600" } else { "bg-zinc-300 dark:bg-zinc-600" })>
                <span class=move || format!("absolute left-0.5 top-0.5 size-3 rounded-full bg-white shadow-sm transition-transform {}", if on.get() { "translate-x-3" } else { "" })></span>
            </span>
        </button>
    }
}

/// Label is required and serves both nonvisual users and the tooltip.
#[component]
pub fn IconButton(
    name: Icon,
    #[prop(into)] label: String,
    on_click: impl Fn(leptos::ev::MouseEvent) + 'static,
    #[prop(optional, into)] disabled: Signal<bool>,
) -> impl IntoView {
    view! {
        <button type="button" class=BAR_BUTTON aria-label=label.clone() title=label
            disabled=move || disabled.get() on:click=on_click>{icon(name, Size::S16)}</button>
    }
}

#[derive(Clone, Copy, Default)]
pub enum Tone {
    #[default]
    Neutral,
    Blue,
    Sky,
    Amber,
    Red,
}

impl Tone {
    fn class(self) -> &'static str {
        match self {
            Self::Neutral => "bg-zinc-100 text-zinc-700 dark:bg-zinc-800 dark:text-zinc-300",
            Self::Blue => "bg-blue-100 text-blue-800 dark:bg-blue-950 dark:text-blue-300",
            Self::Sky => "bg-sky-100 text-sky-800 dark:bg-sky-950 dark:text-sky-300",
            Self::Amber => "bg-amber-100 text-amber-800 dark:bg-amber-950 dark:text-amber-300",
            Self::Red => "bg-red-100 text-red-800 dark:bg-red-950 dark:text-red-300",
        }
    }
}

/// Status vocabulary is supplied by the caller; compact retains it as a title.
#[component]
pub fn Pill(#[prop(into)] text: String, #[prop(optional)] tone: Tone, #[prop(optional)] compact: bool) -> impl IntoView {
    let title = text.clone();
    view! {
        <span class=format!("inline-flex h-5 items-center gap-1.5 rounded-full px-2 text-xs {}", tone.class()) title=title>
            <span aria-hidden="true" class="size-1.5 rounded-full bg-current"></span>
            {if compact { view! { <span class="sr-only">{text}</span> }.into_any() } else { text.into_any() }}
        </span>
    }
}

#[component]
pub fn Badge(n: u32, #[prop(optional)] tone: Tone) -> impl IntoView {
    view! { <span class=format!("inline-flex h-4 min-w-4 items-center justify-center rounded-full px-1 text-[11px] font-medium {}", tone.class())>{n}</span> }
}

/// A picker/reference's contents. An interactive caller owns the button/link;
/// this span can also be used for noninteractive references without fake roles.
#[component]
pub fn Chip(
    #[prop(into)] text: String,
    #[prop(optional)] name: Option<Icon>,
    #[prop(optional)] picker: bool,
    #[prop(optional)] tone: Tone,
) -> impl IntoView {
    view! {
        <span class=format!("inline-flex h-6 max-w-full items-center gap-1 rounded-md px-1.5 text-xs pointer-coarse:min-h-8 {}", tone.class())>
            {name.map(|name| icon(name, Size::S14))}<span class="truncate">{text}</span>
            {picker.then(|| icon(Icon::ChevronDown, Size::S14))}
        </span>
    }
}

#[component]
pub fn Disclosure(#[prop(into)] open: Signal<bool>) -> impl IntoView {
    view! { <span aria-hidden="true" class=move || if open.get() { "inline-flex rotate-90 transition-transform" } else { "inline-flex transition-transform" }>{icon(Icon::ChevronRight, Size::S14)}</span> }
}

#[component]
pub fn EmptyState(#[prop(into)] text: String, #[prop(optional)] action: Option<Children>) -> impl IntoView {
    view! { <div class="py-8 text-center text-sm text-muted"><p>{text}</p>{action.map(|action| view! { <div class="mt-2">{action()}</div> })}</div> }
}

#[component]
pub fn Kbd(#[prop(into)] text: String) -> impl IntoView {
    view! { <kbd class="inline-flex items-center rounded border border-zinc-300 px-1 font-sans text-[11px] leading-4 text-muted dark:border-zinc-700">{text}</kbd> }
}

/// Small, hidden specimen route; the real browser's theme and pointer media
/// query apply, so the same controls can be checked in all four combinations.
#[component]
pub fn Gallery() -> impl IntoView {
    let on = RwSignal::new(false);
    view! {
        <main class="min-h-dvh bg-white p-6 text-zinc-900 dark:bg-zinc-950 dark:text-zinc-100">
            <div class="mx-auto max-w-3xl space-y-6">
                <h1 class="text-xl font-semibold">"Icons and components"</h1>
                <p class="text-sm text-muted">"Stage 1 foundations. Change theme and pointer mode to inspect the same controls."</p>
                <section class="space-y-2"><h2 class=SECTION_LABEL>"Controls"</h2>
                    <Switch on=move || on.get() on_change=move |v| on.set(v) label="Immediate setting" hint="Label left, switch at the row’s end" />
                    <Switch on=Signal::stored(true) on_change=|_| {} label="Disabled setting" disabled=Signal::stored(true) />
                    <label class="flex min-h-8 items-center gap-2 pointer-coarse:min-h-11"><input type="checkbox" class=CHECKBOX />"Submitted choice"</label>
                    <input class=INPUT aria-label="Example input" placeholder="Input" />
                    <select class=SELECT aria-label="Example select"><option>"One"</option><option>"Two"</option></select>
                </section>
                <section class="space-y-2"><h2 class=SECTION_LABEL>"Buttons"</h2>
                    <div class="flex flex-wrap gap-2">{[BTN_PRIMARY, BTN_SECONDARY, BTN_QUIET, BTN_DANGER, BTN_DANGER_SOLID].into_iter().enumerate().map(|(n, tier)| view! { <button class=tier>{["Primary", "Secondary", "Quiet", "Danger", "Confirm danger"][n]}</button> }).collect_view()}
                        <button class=format!("{BTN_PRIMARY} {BTN_SM}")>"Small"</button><button class=format!("{BTN_QUIET} {BTN_MD}") disabled>"Disabled"</button>
                        <IconButton name=Icon::X label="Close example" on_click=|_| {} />
                    </div>
                </section>
                <section class="space-y-2"><h2 class=SECTION_LABEL>"Status, counts and references"</h2>
                    <div class="flex flex-wrap items-center gap-2"><Pill text="idle" /><Pill text="running" tone=Tone::Blue /><Pill text="needs you" tone=Tone::Amber /><Pill text="failed" tone=Tone::Red /><Pill text="running" tone=Tone::Blue compact=true />
                        <Badge n=2 tone=Tone::Amber /><Badge n=3 tone=Tone::Sky /><Badge n=4 /><Chip text="Model" name=Icon::Bot picker=true /><Chip text="Reference" tone=Tone::Sky /><Disclosure open=move || on.get() /><Kbd text="⌘K" />
                    </div><EmptyState text="Nothing here yet." />
                </section>
                <section class="space-y-2"><h2 class=SECTION_LABEL>"Lucide 1.66.0, 14 / 16 / 20px, absolute 1.5px stroke"</h2>
                    <div class="grid grid-cols-2 gap-3 sm:grid-cols-3">{Icon::ALL.iter().copied().map(|name| view! {
                        <div class="flex items-center gap-2 text-xs"><span class="flex items-center gap-1">{[Size::S14, Size::S16, Size::S20].map(|size| icon(name, size))}</span><span>{name.name()}</span></div>
                    }).collect_view()}</div>
                </section>
            </div>
        </main>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn foundations_have_accessible_names_and_trailing_switches() {
        let html = view! { <Switch on=Signal::stored(true) on_change=|_| {} label="Catch-up" hint="A card when you return" /> }.to_html();
        assert!(html.contains("role=\"switch\"") && html.contains("aria-checked=\"true\""), "{html}");
        assert!(html.find("Catch-up</span>").unwrap() < html.find("aria-hidden=\"true\"").unwrap(), "{html}");
        assert!(html.contains("pointer-coarse:min-h-11"), "{html}");
        let html = view! { <IconButton name=Icon::X label="Close" on_click=|_| {} /> }.to_html();
        assert!(html.contains("aria-label=\"Close\"") && html.contains("title=\"Close\""), "{html}");
        let html = view! { <Pill text="running" compact=true /><Kbd text="⌘K" /> }.to_html();
        assert!(html.contains("sr-only") && html.contains("running") && html.contains("<kbd"), "{html}");
    }

    /// Deliberately small, stage-1 guard. The legacy button clusters stay until
    /// step 5; do not pretend to ban them while they remain throughout chrome.
    #[test]
    fn switches_and_checkbox_styles_have_one_home() {
        fn check(path: &std::path::Path) {
            if path.is_dir() {
                for entry in std::fs::read_dir(path).unwrap() {
                    check(&entry.unwrap().path());
                }
                return;
            }
            if path.extension().is_none_or(|ext| ext != "rs") || path.ends_with("components.rs") {
                return;
            }
            let source = std::fs::read_to_string(path).unwrap();
            let app = source.split("#[cfg(test)]").next().unwrap();
            assert!(!app.contains("role=\"switch\""), "{}: use components::Switch", path.display());
            assert!(!app.contains("const SELECT:"), "{}: use components::SELECT", path.display());
            for checkbox in app.split("<input").skip(1).filter_map(|input| input.split_once("/>").map(|(tag, _)| tag)) {
                if checkbox.contains("type=\"checkbox\"") {
                    assert!(checkbox.contains("components::CHECKBOX"), "{}: style a submitted choice with CHECKBOX", path.display());
                }
            }
        }
        check(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
    }
}
