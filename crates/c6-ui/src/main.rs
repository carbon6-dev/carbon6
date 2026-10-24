//! carbon6 web UI (Leptos CSR). Talks only to the head's `/api/v1`.

mod activity;
mod analytics;
mod api;
mod assistant;
mod auth;
mod calendar;
mod collections;
mod contacts;
mod decisions;
mod explorer;
mod files;
mod fleet;
mod itemviews;
mod labels;
mod mail;
mod markdown;
mod memex;
mod notify;
mod outputs;
mod photos;
mod push;
mod search;
mod session;
mod sessions;
mod settings;
mod tasks;
mod term;
mod understanding;
mod usage;
mod util;
mod views;
mod workspace;

use leptos::prelude::*;
use leptos_router::components::{Route, Router, Routes};
use leptos_router::path;
use workspace::{Go, View};

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}

/// Signs the browser in first: until then only the sign-in pages render, so
/// nothing else talks to the head.
#[component]
fn App() -> impl IntoView {
    let status = LocalResource::new(api::auth_status);
    let signed_out = api::signed_out();
    view! {
        <Router>
            {move || match status.get() {
                None => ().into_any(),
                Some(Ok(s)) if s.authenticated && !signed_out.get() => view! { <Main /> }.into_any(),
                Some(Ok(s)) => view! {
                    <Routes fallback=move || view! { <auth::SignIn status=s.clone() /> }>
                        <Route path=path!("/login/:token") view=auth::LinkSignIn />
                    </Routes>
                }.into_any(),
                Some(Err(e)) => view! { <div class="p-8"><util::ErrorBox message=format!("Cannot reach the head: {e}") /></div> }.into_any(),
            }}
        </Router>
    }
}

#[component]
fn Main() -> impl IntoView {
    let live = api::LiveFeed::connect();
    provide_context(live);
    provide_context(labels::LabelFilter::restored());
    // A signed-in browser opening a login link (to sign in again).
    if window().location().pathname().unwrap_or_default().starts_with("/login/") {
        return view! {
            <Routes fallback=|| ()>
                <Route path=path!("/login/:token") view=auth::LinkSignIn />
            </Routes>
        }
        .into_any();
    }
    let ws = workspace::Workspace::restore();
    provide_context(ws);
    ws.watch(live);
    notify::watch_sessions(live, ws, labels::use_filter());
    // Needing attention, as the label filter shows it.
    let filter = labels::use_filter().0;
    let counts = Memo::new(move |_| labels::attention_counts(ws, live, &filter.get()));
    let attention = move || counts.get().0;
    let tiles = move || ws.layout.with(|l| l.tiles.len());
    view! {
        <div class="flex h-dvh flex-col">
            <header class="flex h-11 shrink-0 items-center gap-2 border-b border-zinc-200 bg-zinc-50 px-3 dark:border-zinc-800 dark:bg-zinc-950">
                <Brand />
                <MobileMenu attention=Signal::derive(attention) />
                <nav class="hidden min-w-0 items-center gap-0.5 text-sm md:flex">
                    <NavLink to=View::Sessions label="Sessions" />
                    <NavLink to=View::Workers label="Workers" />
                    <Go to=View::Inbox class=NAV>
                        <span title=move || { let (s, h) = counts.get(); labels::attention_title(s, h) }>
                            "Inbox"
                            {move || (attention() > 0).then(|| view! { <span class="ml-1 rounded-full bg-amber-500 px-1.5 text-[10px] font-semibold text-white">{attention()}</span> })}
                        </span>
                    </Go>
                    <decisions::DecisionLink class=NAV />
                    <NavLink to=View::Analytics label="Analytics" />
                    <span class="hidden xl:contents"><NavLink to=View::Mission label="Mission" /></span>
                    <span class="hidden xl:contents"><NavLink to=View::Collections { name: String::new() } label="Collections" /></span>
                    <span class="hidden xl:contents"><NavLink to=View::Realms label="Realms" /></span>
                    <NavLink to=View::Settings { note: None, section: String::new() } label="Settings" />
                </nav>
                <span class="flex-1"></span>
                <span class="hidden xl:contents"><views::ViewsMenu class=NAV /></span>
                <RealmSwitch />
                <assistant::AskButton />
                <span class="hidden md:contents"><search::SearchBox /></span>
                {move || (tiles() < workspace::MAX_TILES).then(|| view! {
                    <button class="hidden rounded-md px-2 py-1 text-xs text-zinc-500 hover:bg-zinc-200/60 hover:text-zinc-900 xl:inline dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
                        title="New tile (or ⌘-click any link) · ⌘1–4 switch tiles · ⌘[ ⌘] back and forward"
                        on:click=move |_| ws.new_tile()>"+ Tile"</button>
                })}
                <notify::NotifyToggle />
                <ConnBadge connected=live.connected />
            </header>
            <UpdatedBanner />
            <div class="flex min-h-0 flex-1">
                <activity::Dock />
                <workspace::Tiles />
            </div>
            <activity::MobileBar />
        </div>
    }
    .into_any()
}

/// Phones and narrower windows: the nav, views and search in a menu.
#[component]
fn MobileMenu(attention: Signal<usize>) -> impl IntoView {
    let menu = NodeRef::<leptos::html::Details>::new();
    // Mail, Calendar and Contacts are apps: shown where one is installed.
    let feeds = LocalResource::new(move || {
        api::realm().track();
        api::rpc("memex.feeds", serde_json::json!({}))
    });
    // Close after choosing something.
    let close = move |_| {
        if let Some(d) = menu.get_untracked() {
            d.set_open(false);
        }
    };
    view! {
        <details node_ref=menu class="relative xl:hidden">
            <summary class="cursor-pointer list-none rounded-md px-2 py-1 text-sm text-zinc-600 dark:text-zinc-400">
                "☰"
                {move || (attention.get() > 0).then(|| view! { <span class="ml-1 rounded-full bg-amber-500 px-1.5 text-[10px] font-semibold text-white">{attention.get()}</span> })}
            </summary>
            <div class="absolute left-0 top-9 z-40 w-64 space-y-1 rounded-lg border border-zinc-200 bg-white p-2 shadow-lg dark:border-zinc-700 dark:bg-zinc-900" on:click=close>
                <div on:click=|ev| ev.stop_propagation()><search::SearchBox /></div>
                <div class="flex flex-col text-sm">
                    <Go to=View::Sessions class=MENU>"Sessions"</Go>
                    <Go to=View::Inbox class=MENU>"Inbox"</Go>
                    <decisions::DecisionLink class=MENU />
                    <Go to=View::Mission class=MENU>"Mission control"</Go>
                    <Go to=View::Workers class=MENU>"Workers"</Go>
                    <Go to=View::Collections { name: String::new() } class=MENU>"Collections"</Go>
                    <Go to=View::Realms class=MENU>"Realms"</Go>
                    <views::ViewsMenu class=MENU />
                    // Only what an app feeds in this realm (or it holds).
                    {move || feeds.get().and_then(Result::ok).map(|f| view! {
                        {(f["mail"] == true).then(|| view! { <Go to=View::Mail class=MENU>"Mail"</Go> })}
                        {(f["calendar"] == true).then(|| view! { <Go to=View::Calendar class=MENU>"Calendar"</Go> })}
                        {(f["contacts"] == true).then(|| view! { <Go to=View::Contacts class=MENU>"Contacts"</Go> })}
                    })}
                    <Go to=View::Photos class=MENU>"Photos"</Go>
                    <Go to=View::Memex class=MENU>"Memex"</Go>
                    <Go to=View::Analytics class=MENU>"Analytics"</Go>
                    <Go to=View::Usage class=MENU>"Usage and budgets"</Go>
                    <Go to=View::Settings { note: None, section: String::new() } class=MENU>"Settings"</Go>
                </div>
            </div>
        </details>
    }
}

const MENU: &str = "rounded-md px-2 py-2 hover:bg-zinc-100 dark:hover:bg-zinc-800";

const NAV: &str = "shrink-0 rounded-md px-2 py-1 text-zinc-600 hover:bg-zinc-200/60 hover:text-zinc-900 dark:text-zinc-400 dark:hover:bg-zinc-800 dark:hover:text-zinc-100";

#[component]
fn NavLink(to: View, label: &'static str) -> impl IntoView {
    view! { <Go to class=NAV>{label}</Go> }
}

#[component]
fn ConnBadge(connected: ReadSignal<bool>) -> impl IntoView {
    view! {
        <span class="flex items-center gap-1.5 text-xs text-zinc-500">
            <span class=move || {
                if connected.get() { "h-2 w-2 rounded-full bg-emerald-500" } else { "h-2 w-2 animate-pulse rounded-full bg-amber-500" }
            }></span>
            <span class="hidden sm:inline">{move || if connected.get() { "live" } else { "reconnecting…" }}</span>
        </span>
    }
}

/// Which realm you are looking at (docs/os.md §3): sessions, decisions and
/// collections show its own, new sessions start in it, and Settings edits
/// its lists. Remembered in this browser.
#[component]
fn RealmSwitch() -> impl IntoView {
    let realms = LocalResource::new(move || {
        api::realms_changed().track();
        api::realms()
    });
    let current = api::realm();
    view! {
        {move || realms.get().and_then(Result::ok).filter(|rs| rs.iter().filter(|r| r.id != "root").count() > 1).map(|rs| view! {
            <select class="hidden rounded-md border border-zinc-300 bg-white px-1 py-0.5 text-xs md:block dark:border-zinc-700 dark:bg-zinc-900"
                title="The realm you are looking at" prop:value=move || current.get()
                on:change=move |ev| {
                    let v = event_target_value(&ev);
                    util::storage_set("c6.realm", &v);
                    current.set(v);
                }>
                <option value="">"All realms"</option>
                {rs.into_iter().filter(|r| r.id != "root").map(|r| view! { <option value=r.id.clone()>{r.name.clone()}</option> }).collect_view()}
            </select>
        })}
    }
}

/// carbon6.dev's mark (site/static/icon.svg), drawn inline.
const MARK: &str = r##"<svg viewBox="0 0 192 192" width="22" height="22" aria-hidden="true"><rect width="192" height="192" rx="42" fill="#0b0b0c"/><path d="M151.8 100.9A56 56 0 1 1 121.4 46.1" fill="none" stroke="#38bdf8" stroke-width="22"/><circle cx="146" cy="67" r="12" fill="#f5f5f5"/></svg>"##;

/// The head's version as it fits in the header: the release, or for an
/// edge build of main `0.3.11 edge·c048da0` (the full one on hover).
fn short_version(v: &str) -> String {
    match v.split_once("+git") {
        Some((release, rest)) => format!("{release} edge·{}", rest.rsplit('.').next().unwrap_or("")),
        None => v.to_owned(),
    }
}

/// The version the head ran when this page loaded, and what it runs now.
#[derive(Clone, Copy)]
struct Versions {
    loaded: RwSignal<Option<String>>,
    now: RwSignal<Option<String>>,
}

fn versions() -> Versions {
    match use_context::<Versions>() {
        Some(v) => v,
        None => {
            let v = Versions { loaded: RwSignal::new(None), now: RwSignal::new(None) };
            provide_context(v);
            // Asked when the page loads, every 30 seconds, and whenever the
            // live connection comes back (as it does after the head restarts).
            let ask = move || {
                leptos::task::spawn_local(async move {
                    if let Ok(now) = api::version().await {
                        if v.loaded.get_untracked().is_none() {
                            v.loaded.set(Some(now.clone()));
                        }
                        v.now.set(Some(now));
                    }
                });
            };
            ask();
            let live = api::live();
            Effect::new(move |was: Option<bool>| {
                let up = live.connected.get();
                if up && was == Some(false) {
                    ask();
                }
                up
            });
            let _ = set_interval_with_handle(ask, std::time::Duration::from_secs(30));
            v
        }
    }
}

/// The mark, the name and the version, always whole.
#[component]
fn Brand() -> impl IntoView {
    let v = versions();
    view! {
        <Go to=View::Sessions class="mr-2 flex shrink-0 items-center gap-2 whitespace-nowrap">
            <span class="shrink-0" inner_html=MARK></span>
            <span class="font-semibold tracking-tight">"carbon6"</span>
            {move || v.now.get().map(|full| view! {
                <span class="rounded bg-zinc-200/70 px-1.5 py-0.5 font-mono text-[10px] text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400" title=format!("carbon6 {full}")>
                    {short_version(&full)}
                </span>
            })}
        </Go>
    }
}

/// When the head now runs another version than this page was loaded from,
/// says so and offers to reload (the page is the old version's).
#[component]
fn UpdatedBanner() -> impl IntoView {
    let v = versions();
    let dismissed = RwSignal::new(None::<String>);
    let changed = move || match (v.loaded.get(), v.now.get()) {
        (Some(a), Some(b)) if a != b && dismissed.get().as_deref() != Some(b.as_str()) => Some((a, b)),
        _ => None,
    };
    view! {
        {move || changed().map(|(was, now)| {
            let n2 = now.clone();
            view! {
                <div class="flex shrink-0 items-center gap-3 border-b border-sky-200 bg-sky-50 px-3 py-1.5 text-sm text-sky-900 dark:border-sky-900 dark:bg-sky-950 dark:text-sky-200">
                    <span class="min-w-0 flex-1">{format!("carbon6 was updated to {} (this page is {}). Reload to use the new version.", short_version(&now), short_version(&was))}</span>
                    <button class="rounded-md bg-sky-600 px-3 py-1 text-xs font-medium text-white hover:bg-sky-700" on:click=move |_| { let _ = window().location().reload(); }>"Reload"</button>
                    <button class="rounded px-1.5 text-sky-700 hover:bg-sky-100 dark:text-sky-300 dark:hover:bg-sky-900" title="Not now" on:click=move |_| dismissed.set(Some(n2.clone()))>"✕"</button>
                </div>
            }
        })}
    }
}
