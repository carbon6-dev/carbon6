//! carbon6 web UI (Leptos CSR). Talks only to the head's `/api/v1`.

mod activity;
mod analytics;
mod api;
mod assistant;
mod auth;
mod calendar;
mod chat;
mod collections;
mod contacts;
mod decisions;
mod details;
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
mod place;
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
    // Each page is one realm's app or the console (docs/realms-ux.md).
    if place::resolve() {
        leptos::mount::mount_to_body(App);
    }
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
    let console = place::is_console();
    view! {
        <div class="flex h-dvh flex-col">
            <header class="flex h-11 shrink-0 items-center gap-2 border-b border-zinc-200 bg-zinc-50 px-3 dark:border-zinc-800 dark:bg-zinc-950">
                <Brand />
                <MobileMenu attention=Signal::derive(attention) />
                <PlaceMenu />
                {if console {
                    view! {
                        <nav class="hidden min-w-0 items-center gap-0.5 text-sm md:flex">
                            <NavLink to=View::Realms label="Realms" />
                            <decisions::DecisionLink class=NAV />
                            <NavLink to=View::Workers label="Workers" />
                            <NavLink to=View::Analytics label="Analytics" />
                            <NavLink to=View::Usage label="Spending" />
                            <NavLink to=View::Settings { note: None, section: String::new() } label="Settings" />
                        </nav>
                        <span class="flex-1"></span>
                        <assistant::AskButton />
                    }.into_any()
                } else {
                    view! {
                        <nav class="hidden min-w-0 items-center gap-0.5 text-sm md:flex">
                            <NavLink to=View::Sessions label="Sessions" />
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
                            <span class="hidden xl:contents"><NavLink to=View::Workers label="Workers" /></span>
                            <NavLink to=View::Settings { note: None, section: String::new() } label="Settings" />
                        </nav>
                        <span class="flex-1"></span>
                        <span class="hidden xl:contents"><views::ViewsMenu class=NAV /></span>
                        <assistant::AskButton />
                        <span class="hidden md:contents"><search::SearchBox /></span>
                    }.into_any()
                }}
                {move || (tiles() < workspace::MAX_TILES).then(|| view! {
                    <button class="hidden rounded-md px-2 py-1 text-xs text-zinc-500 hover:bg-zinc-200/60 hover:text-zinc-900 xl:inline dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
                        title="New tile (or ⌘-click any link) · ⌘1–4 switch tiles · ⌘[ ⌘] back and forward"
                        on:click=move |_| ws.new_tile()>"+ Tile"</button>
                })}
                {(!console).then(|| view! { <notify::NotifyToggle /> })}
                <ConnBadge connected=live.connected />
            </header>
            <UpdatedBanner />
            <MissingRealm />
            <div class="flex min-h-0 flex-1">
                {(!console).then(|| view! { <activity::Dock /> })}
                <workspace::Tiles />
            </div>
            {(!console).then(|| view! { <activity::MobileBar /> })}
        </div>
    }
    .into_any()
}

/// Phones and narrower windows: the nav, views and search in a menu.
#[component]
fn MobileMenu(attention: Signal<usize>) -> impl IntoView {
    let menu = NodeRef::<leptos::html::Details>::new();
    // Mail, Calendar and Contacts are apps: shown where one is installed.
    // A realm's mail, calendar and contacts (the console has none).
    let feeds = LocalResource::new(move || {
        api::realm().track();
        async move {
            if place::is_console() {
                return Err(String::new());
            }
            api::rpc("memex.feeds", serde_json::json!({})).await
        }
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
            <div class="fixed inset-x-2 top-12 z-40 max-h-[80dvh] space-y-1 overflow-y-auto rounded-lg border border-zinc-200 bg-white p-2 shadow-lg sm:absolute sm:inset-x-auto sm:left-0 sm:top-9 sm:w-64 dark:border-zinc-700 dark:bg-zinc-900" on:click=close>
                {place::is_console().then(|| view! {
                    <div class="flex flex-col text-sm">
                        <Go to=View::Realms class=MENU>"Realms"</Go>
                        <decisions::DecisionLink class=MENU />
                        <Go to=View::Workers class=MENU>"Workers"</Go>
                        <Go to=View::Analytics class=MENU>"Analytics"</Go>
                        <Go to=View::Usage class=MENU>"Spending"</Go>
                        <Go to=View::Settings { note: None, section: String::new() } class=MENU>"Settings"</Go>
                    </div>
                })}
                <div class:hidden=place::is_console() on:click=|ev| ev.stop_propagation()><search::SearchBox /></div>
                <div class="flex flex-col text-sm" class:hidden=place::is_console()>
                    <Go to=View::Sessions class=MENU>"Sessions"</Go>
                    <Go to=View::Inbox class=MENU>"Inbox"</Go>
                    <decisions::DecisionLink class=MENU />
                    <Go to=View::Mission class=MENU>"Mission control"</Go>
                    <Go to=View::Workers class=MENU>"Workers"</Go>
                    <Go to=View::Collections { name: String::new() } class=MENU>"Collections"</Go>
                    <views::ViewsMenu class=MENU />
                    // Only what an app feeds in this realm (or it holds).
                    {move || feeds.get().and_then(Result::ok).map(|f| view! {
                        {(f["mail"] == true).then(|| view! { <Go to=View::Mail class=MENU>"Mail"</Go> })}
                        {(f["chat"] == true).then(|| view! { <Go to=View::Chat class=MENU>"Chat"</Go> })}
                        {(f["calendar"] == true).then(|| view! { <Go to=View::Calendar class=MENU>"Calendar"</Go> })}
                        {(f["contacts"] == true).then(|| view! { <Go to=View::Contacts class=MENU>"Contacts"</Go> })}
                    })}
                    <Go to=View::Photos class=MENU>"Photos"</Go>
                    <Go to=View::Memex class=MENU>"Memex"</Go>
                    <Go to=View::Analytics class=MENU>"Analytics"</Go>
                    <Go to=View::Usage class=MENU>"Usage and budgets"</Go>
                    <Go to=View::Settings { note: None, section: String::new() } class=MENU>"Settings"</Go>
                </div>
                <PlaceLinks />
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

/// A realm's colour, the same everywhere it shows.
fn realm_colour(id: &str) -> &'static str {
    const OTHERS: [&str; 6] = ["bg-emerald-500", "bg-violet-500", "bg-rose-500", "bg-teal-500", "bg-indigo-500", "bg-lime-500"];
    match id {
        "home" => "bg-sky-500",
        "commons" => "bg-amber-500",
        _ => OTHERS[id.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619)) as usize % OTHERS.len()],
    }
}

/// Where you are (a realm, in its colour, or the console) and the way to
/// the others, each with how many things wait on you there
/// (docs/realms-ux.md §2, §6). Each is a link: ⌘-click opens it in a new
/// tab. Links to a place are `rel="external"`: a place is fixed when the
/// page loads, so the router must not handle them in the page.
#[component]
fn PlaceMenu() -> impl IntoView {
    let places = LocalResource::new(move || {
        api::realms_changed().track();
        api::elsewhere()
    });
    let menu = NodeRef::<leptos::html::Details>::new();
    let here = place::realm();
    let elsewhere = {
        let here = here.clone();
        // The console's too, unless you're in it.
        move || {
            places.get().and_then(Result::ok).map_or(0, |ps| {
                ps.iter()
                    .filter(|p| if p.id == "root" { !place::is_console() } else { Some(&p.id) != here.as_ref() })
                    .map(|p| p.waiting)
                    .sum::<u64>()
            })
        }
    };
    let name = {
        let here = here.clone();
        move || match &here {
            Some(r) => places.get().and_then(Result::ok).and_then(|ps| ps.into_iter().find(|p| &p.id == r).map(|p| p.name)).unwrap_or_else(|| r.clone()),
            None => "Console".into(),
        }
    };
    let dot = here.as_deref().map_or("bg-zinc-900 dark:bg-zinc-100", realm_colour);
    view! {
        <details node_ref=menu class="relative">
            <summary class="flex cursor-pointer list-none items-center gap-1.5 rounded-md border border-zinc-300 px-2 py-0.5 text-sm dark:border-zinc-700"
                title="Where you are: a realm, or the console">
                <span class=format!("h-2.5 w-2.5 shrink-0 rounded-full {dot}")></span>
                <span class="max-w-32 truncate font-medium">{name}</span>
                {move || (elsewhere() > 0).then(|| view! {
                    <span class="rounded-full bg-amber-500 px-1.5 text-[10px] font-semibold text-white" title="Waiting on you in other realms">{elsewhere()}</span>
                })}
                <span class="text-zinc-400">"▾"</span>
            </summary>
            // On a phone, the width of the screen (anchored at the chip, it
            // would run off its edge).
            <div class="fixed inset-x-2 top-12 z-40 rounded-lg border border-zinc-200 bg-white p-1 text-sm shadow-lg sm:absolute sm:inset-x-auto sm:left-0 sm:top-9 sm:w-64 dark:border-zinc-700 dark:bg-zinc-900">
                <div class="px-2 py-1 text-[11px] font-semibold uppercase tracking-wide text-zinc-500">"Realms"</div>
                {move || places.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|p| p.id != "root").map(|p| {
                    let current = here.as_deref() == Some(p.id.as_str());
                    view! {
                        <a rel="external" href=place::url_of(&place::Place::Realm(p.id.clone()))
                            class=format!("flex items-center gap-2 rounded-md px-2 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800 {}", if current { "font-medium" } else { "" })>
                            <span class=format!("h-2.5 w-2.5 shrink-0 rounded-full {}", realm_colour(&p.id))></span>
                            <span class="min-w-0 flex-1 truncate">{p.name.clone()}</span>
                            {(p.waiting > 0).then(|| view! { <span class="rounded-full bg-amber-500 px-1.5 text-[10px] font-semibold text-white" title="Waiting on you">{p.waiting}</span> })}
                            {current.then(|| view! { <span class="text-zinc-400">"✓"</span> })}
                        </a>
                    }
                }).collect_view()}
                <div class="my-1 border-t border-zinc-200 dark:border-zinc-800"></div>
                <a rel="external" href=place::url_of(&place::Place::Console) class="flex items-center gap-2 rounded-md px-2 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800">
                    <span class="h-2.5 w-2.5 shrink-0 rounded-full bg-zinc-900 dark:bg-zinc-100"></span>
                    <span class="flex-1">"Console"</span>
                    {move || console_waiting(&places).map(|n| view! { <span class="rounded-full bg-amber-500 px-1.5 text-[10px] font-semibold text-white" title="Waiting on you: installs, grants">{n}</span> })}
                    <span class="text-xs text-zinc-500">"realms, workers, system"</span>
                </a>
            </div>
        </details>
    }
}

/// What waits on you in the console (root's: installs, grants), if any.
fn console_waiting(places: &LocalResource<Result<Vec<c6_api::RealmWaiting>, String>>) -> Option<u64> {
    places.get().and_then(Result::ok)?.iter().find(|p| p.id == "root").map(|p| p.waiting).filter(|n| *n > 0)
}

/// The realms and the console, as links (the phone's ☰ menu).
#[component]
fn PlaceLinks() -> impl IntoView {
    let places = LocalResource::new(api::elsewhere);
    let here = place::realm();
    view! {
        <div class="border-t border-zinc-200 pt-1 text-sm dark:border-zinc-800">
            <div class="px-2 py-1 text-[11px] font-semibold uppercase tracking-wide text-zinc-500">"Realms"</div>
            {move || places.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|p| p.id != "root").map(|p| {
                let current = here.as_deref() == Some(p.id.as_str());
                view! {
                    <a rel="external" href=place::url_of(&place::Place::Realm(p.id.clone())) class=format!("flex items-center gap-2 {MENU} {}", if current { "font-medium" } else { "" })>
                        <span class=format!("h-2.5 w-2.5 shrink-0 rounded-full {}", realm_colour(&p.id))></span>
                        <span class="min-w-0 flex-1 truncate">{p.name.clone()}</span>
                        {(p.waiting > 0).then(|| view! { <span class="rounded-full bg-amber-500 px-1.5 text-[10px] font-semibold text-white">{p.waiting}</span> })}
                        {current.then(|| view! { <span class="text-zinc-400">"✓"</span> })}
                    </a>
                }
            }).collect_view()}
            <a rel="external" href=place::url_of(&place::Place::Console) class=format!("flex items-center gap-2 {MENU} {}", if place::is_console() { "font-medium" } else { "" })>
                <span class="h-2.5 w-2.5 shrink-0 rounded-full bg-zinc-900 dark:bg-zinc-100"></span>
                <span class="flex-1">"Console"</span>
                {move || console_waiting(&places).map(|n| view! { <span class="rounded-full bg-amber-500 px-1.5 text-[10px] font-semibold text-white">{n}</span> })}
            </a>
        </div>
    }
}

/// A realm page for a realm that isn't there (deleted, or mistyped).
#[component]
fn MissingRealm() -> impl IntoView {
    let Some(here) = place::realm() else { return ().into_any() };
    let places = LocalResource::new(api::elsewhere);
    view! {
        {move || places.get().and_then(Result::ok).filter(|ps| !ps.iter().any(|p| p.id == here)).map(|ps| view! {
            <div class="fixed inset-0 z-50 flex items-center justify-center bg-zinc-50/95 p-6 dark:bg-zinc-950/95">
                <div class="max-w-md space-y-3 rounded-xl border border-zinc-200 bg-white p-6 dark:border-zinc-800 dark:bg-zinc-900">
                    <h1 class="text-lg font-semibold">{format!("There is no realm {here:?}")}</h1>
                    <p class="text-sm text-zinc-500">"It may have been deleted, or the address is mistyped. Go to one of yours:"</p>
                    <div class="flex flex-wrap gap-2">
                        {ps.into_iter().filter(|p| p.id != "root").map(|p| view! {
                            <a class="rounded-md border border-zinc-300 px-3 py-1 text-sm hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
                                rel="external" href=place::url_of(&place::Place::Realm(p.id.clone()))>{p.name}</a>
                        }).collect_view()}
                        <a class="rounded-md border border-zinc-300 px-3 py-1 text-sm hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800" rel="external" href=place::url_of(&place::Place::Console)>"Console"</a>
                    </div>
                </div>
            </div>
        })}
    }
    .into_any()
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
            <span class="hidden font-semibold tracking-tight sm:inline">"carbon6"</span>
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
