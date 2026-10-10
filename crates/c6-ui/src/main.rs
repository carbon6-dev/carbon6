//! carbon6 web UI (Leptos CSR). Talks only to the head's `/api/v1`.

mod api;
mod fleet;

use leptos::prelude::*;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}

#[component]
fn App() -> impl IntoView {
    let live = api::LiveFeed::connect();
    view! {
        <div class="min-h-screen">
            <header class="border-b border-zinc-200 dark:border-zinc-800">
                <div class="mx-auto max-w-6xl px-4 h-14 flex items-center justify-between">
                    <div class="flex items-center gap-2">
                        <span class="font-semibold tracking-tight">"carbon6"</span>
                        <span class="text-xs text-zinc-500">"head"</span>
                    </div>
                    <ConnBadge connected=live.connected />
                </div>
            </header>
            <main class="mx-auto max-w-6xl px-4 py-6">
                <fleet::Fleet workers=live.workers />
            </main>
        </div>
    }
}

#[component]
fn ConnBadge(connected: ReadSignal<bool>) -> impl IntoView {
    view! {
        <span class="text-xs flex items-center gap-1.5 text-zinc-500">
            <span class=move || {
                if connected.get() { "h-2 w-2 rounded-full bg-emerald-500" } else { "h-2 w-2 rounded-full bg-amber-500 animate-pulse" }
            }></span>
            {move || if connected.get() { "live" } else { "reconnecting…" }}
        </span>
    }
}
