//! carbon6 web UI (Leptos CSR). Talks only to the head's `/api/v1`.

mod activity;
mod api;
mod auth;
mod calendar;
mod contacts;
mod files;
mod fleet;
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
                <Go to=View::Sessions class="mr-2 font-semibold tracking-tight">"carbon6"</Go>
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
                    <span class="hidden sm:contents"><NavLink to=View::Mission label="Mission" /></span>
                    <NavLink to=View::Settings { note: None } label="Settings" />
                </nav>
                <span class="flex-1"></span>
                <span class="hidden md:contents"><search::SearchBox /></span>
                {move || (tiles() < workspace::MAX_TILES).then(|| view! {
                    <button class="hidden rounded-md px-2 py-1 text-xs text-zinc-500 hover:bg-zinc-200/60 hover:text-zinc-900 md:inline dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
                        title="New tile (or ⌘-click any link) · ⌘1–4 switch tiles · ⌘[ ⌘] back and forward"
                        on:click=move |_| ws.new_tile()>"+ Tile"</button>
                })}
                <notify::NotifyToggle />
                <ConnBadge connected=live.connected />
            </header>
            <div class="flex min-h-0 flex-1">
                <activity::Dock />
                <workspace::Tiles />
            </div>
            <activity::MobileBar />
        </div>
    }
    .into_any()
}

/// Phones: the nav and search in a menu.
#[component]
fn MobileMenu(attention: Signal<usize>) -> impl IntoView {
    let menu = NodeRef::<leptos::html::Details>::new();
    // Close after choosing something.
    let close = move |_| {
        if let Some(d) = menu.get_untracked() {
            d.set_open(false);
        }
    };
    view! {
        <details node_ref=menu class="relative md:hidden">
            <summary class="cursor-pointer list-none rounded-md px-2 py-1 text-sm text-zinc-600 dark:text-zinc-400">
                "☰"
                {move || (attention.get() > 0).then(|| view! { <span class="ml-1 rounded-full bg-amber-500 px-1.5 text-[10px] font-semibold text-white">{attention.get()}</span> })}
            </summary>
            <div class="absolute left-0 top-9 z-40 w-64 space-y-1 rounded-lg border border-zinc-200 bg-white p-2 shadow-lg dark:border-zinc-700 dark:bg-zinc-900" on:click=close>
                <div on:click=|ev| ev.stop_propagation()><search::SearchBox /></div>
                <div class="flex flex-col text-sm">
                    <Go to=View::Sessions class=MENU>"Sessions"</Go>
                    <Go to=View::Inbox class=MENU>"Inbox"</Go>
                    <Go to=View::Mission class=MENU>"Mission control"</Go>
                    <Go to=View::Workers class=MENU>"Workers"</Go>
                    <Go to=View::Mail class=MENU>"Mail"</Go>
                    <Go to=View::Calendar class=MENU>"Calendar"</Go>
                    <Go to=View::Contacts class=MENU>"Contacts"</Go>
                    <Go to=View::Photos class=MENU>"Photos"</Go>
                    <Go to=View::Memex class=MENU>"Memex"</Go>
                    <Go to=View::Usage class=MENU>"Usage and budgets"</Go>
                    <Go to=View::Settings { note: None } class=MENU>"Settings"</Go>
                </div>
            </div>
        </details>
    }
}

const MENU: &str = "rounded-md px-2 py-2 hover:bg-zinc-100 dark:hover:bg-zinc-800";

const NAV: &str = "shrink-0 rounded-md px-2.5 py-1 text-zinc-600 hover:bg-zinc-200/60 hover:text-zinc-900 dark:text-zinc-400 dark:hover:bg-zinc-800 dark:hover:text-zinc-100";

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
