//! carbon6 web UI (Leptos CSR). Talks only to the head's `/api/v1`.

mod api;
mod auth;
mod files;
mod fleet;
mod markdown;
mod notify;
mod session;
mod sessions;
mod settings;
mod term;
mod util;

use leptos::prelude::*;
use leptos_router::components::{A, Route, Router, Routes};
use leptos_router::path;

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
    notify::watch_sessions(live);
    view! {
            <div class="min-h-screen">
                <header class="sticky top-0 z-20 border-b border-zinc-200 bg-zinc-50/90 backdrop-blur dark:border-zinc-800 dark:bg-zinc-950/90">
                    <div class="mx-auto flex h-12 max-w-6xl items-center justify-between gap-4 px-4">
                        <nav class="flex items-center gap-1 text-sm">
                            <A href="/" attr:class="mr-3 font-semibold tracking-tight">"carbon6"</A>
                            <NavLink href="/" label="Sessions" />
                            <NavLink href="/workers" label="Workers" />
                            <NavLink href="/settings" label="Settings" />
                        </nav>
                        <div class="flex items-center gap-3">
                            <notify::NotifyToggle />
                            <ConnBadge connected=live.connected />
                        </div>
                    </div>
                </header>
                <Routes fallback=|| view! { <p class="p-8 text-zinc-500">"Not found"</p> }>
                    <Route path=path!("/") view=sessions::SessionsPage />
                    <Route path=path!("/sessions/:id") view=session::SessionPage />
                    <Route path=path!("/workers") view=fleet::FleetPage />
                    <Route path=path!("/workers/:id") view=fleet::WorkerPage />
                    <Route path=path!("/settings") view=settings::SettingsPage />
                    <Route path=path!("/login/:token") view=auth::LinkSignIn />
                </Routes>
            </div>
    }
}

#[component]
fn NavLink(href: &'static str, label: &'static str) -> impl IntoView {
    view! {
        <A href=href attr:class="rounded-md px-2.5 py-1 text-zinc-600 hover:bg-zinc-200/60 hover:text-zinc-900 aria-[current=page]:bg-zinc-200 aria-[current=page]:text-zinc-900 dark:text-zinc-400 dark:hover:bg-zinc-800 dark:hover:text-zinc-100 dark:aria-[current=page]:bg-zinc-800 dark:aria-[current=page]:text-zinc-100">
            {label}
        </A>
    }
}

#[component]
fn ConnBadge(connected: ReadSignal<bool>) -> impl IntoView {
    view! {
        <span class="flex items-center gap-1.5 text-xs text-zinc-500">
            <span class=move || {
                if connected.get() { "h-2 w-2 rounded-full bg-emerald-500" } else { "h-2 w-2 animate-pulse rounded-full bg-amber-500" }
            }></span>
            {move || if connected.get() { "live" } else { "reconnecting…" }}
        </span>
    }
}
