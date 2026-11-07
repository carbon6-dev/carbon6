//! carbon6 web UI (Leptos CSR). Talks only to the head's `/api/v1`.

mod activity;
mod adopt;
mod analytics;
mod api;
mod aspects;
mod assistant;
mod auth;
mod backups;
mod brief;
mod calendar;
mod catchup;
mod chat;
mod chats;
mod collections;
mod components;
mod compose;
mod contacts;
mod decisions;
mod deck;
mod details;
mod docs;
mod explorer;
mod files;
mod fleet;
mod icons;
mod itemviews;
mod labels;
mod mail;
mod mail_html;
mod markdown;
mod marks;
mod memex;
mod mentions;
mod models;
mod monitor;
mod needs;
mod notify;
mod outputs;
mod panes;
mod people;
mod photos;
mod place;
mod projects;
mod push;
mod quota;
mod resources;
mod search;
mod session;
mod session_archive;
mod session_ring;
mod sessions;
mod settings;
mod tasks;
mod term;
mod timeline;
mod understanding;
mod usage;
mod util;
mod views;
mod watchers;
mod when;
mod work;
mod workspace;

use leptos::prelude::*;
use leptos_router::components::{Route, Router, Routes};
use leptos_router::path;
use workspace::{Go, View};

#[wasm_bindgen::prelude::wasm_bindgen]
extern "C" {
    /// js/crash.js: the reload banner; captures a JS stack while the panic's
    /// app-level Wasm callers are still on it (before the subsequent trap).
    #[wasm_bindgen(catch, js_name = c6Crashed)]
    fn js_crashed(message: &str) -> Result<(), wasm_bindgen::JsValue>;

    /// Measure only on size/content changes, before paint; returns cleanup.
    #[cfg(target_arch = "wasm32")]
    #[wasm_bindgen(js_name = c6HeaderFit)]
    fn header_fit(header: &web_sys::Element) -> js_sys::Function;
}

fn main() {
    // A panic leaves the app broken (WebAssembly doesn't unwind: what it
    // held stays held, and its stack is never given back), so beyond the
    // console's "panicked at …" the page says so and offers a reload.
    std::panic::set_hook(Box::new(|info| {
        console_error_panic_hook::hook(info);
        let _ = js_crashed(&info.to_string());
    }));
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
    util::watch_visible_viewport();
    resources::provide();
    provide_context(labels::LabelFilter::restored());
    provide_context(sessions::Grouping::load());
    provide_context(aspects::Facets::load());
    provide_context(settings::TranscriptDisplay::load());
    provide_context(settings::SessionBarDisplay::load());
    provide_context(assistant::AskState(RwSignal::new(false)));
    // Plan limits per connection, beside every connection picker.
    quota::provide();
    // A signed-in browser opening a login link (to sign in again).
    if window().location().pathname().unwrap_or_default().starts_with("/login/") {
        return view! {
            <Routes fallback=|| ()>
                <Route path=path!("/login/:token") view=auth::LinkSignIn />
            </Routes>
        }
        .into_any();
    }
    // A deck presented full screen, or its presenter view (docs/docs.md §5).
    let here = place::strip(&window().location().pathname().unwrap_or_default());
    if here == "/dev/components" {
        return view! { <components::Gallery /> }.into_any();
    }
    if let Some(id) = here.strip_prefix("/docs/").and_then(|r| r.strip_suffix("/present")) {
        let presenter = window().location().search().unwrap_or_default().contains("presenter=1");
        return view! { <deck::Present id=id.to_owned() presenter /> }.into_any();
    }
    let ws = workspace::Workspace::restore();
    provide_context(ws);
    util::Notice::provide();
    session_archive::SessionArchive::provide();
    ws.watch(live);
    notify::watch_sessions(live, ws, labels::use_filter());
    // Needing attention, as the label filter shows it.
    let filter = labels::use_filter().0;
    let counts = Memo::new(move |_| labels::attention_counts(ws, live, &filter.get()));
    let console = place::is_console();
    let tick = decisions::ticking();
    let places = LocalResource::new(move || {
        api::realms_changed().track();
        live.attention.track();
        live.sessions.track();
        tick.track();
        api::elsewhere()
    });
    provide_context(PhoneWaiting(Signal::derive(move || {
        places.get().and_then(Result::ok).map_or(0, |ps| ps.iter().map(|p| p.waiting).sum())
    })));
    view! {
        // The page itself never scrolls: its parts do (a phone panned the
        // whole page, header gone and the right edge cut off).
        <div class="c6-app flex h-dvh flex-col overflow-hidden">
            <util::NoticeHost />
            <assistant::AskHost />
            <session_archive::ArchiveHost />
            <mentions::Peek />
            <DesktopHeader counts />
            <UpdatedBanner />
            <MissingRealm />
            <div class="flex min-h-0 flex-1">
                // Focus mode gives the focused tile the whole width.
                {(!console).then(|| view! { <div class=move || if ws.is_full() { "contents md:hidden" } else { "contents" }><activity::Dock /></div> })}
                <workspace::Tiles />
            </div>
        </div>
    }
    .into_any()
}

/// Content, not a screen breakpoint, decides what fits. Quick navigation and
/// attention stay longest; administration, saved views and build details yield
/// first. The phone drawer remains the complete navigation at small widths.
#[component]
fn DesktopHeader(counts: Memo<(usize, usize)>) -> impl IntoView {
    let node = NodeRef::<leptos::html::Header>::new();
    #[cfg(target_arch = "wasm32")]
    Effect::new(move || {
        if let Some(el) = node.get() {
            let dispose = send_wrapper::SendWrapper::new(header_fit(&el));
            on_cleanup(move || {
                let _ = dispose.call0(&wasm_bindgen::JsValue::NULL);
            });
        }
    });
    let ws = workspace::use_workspace();
    let live = api::live();
    let console = place::is_console();
    let attention = Signal::derive(move || counts.get().0);
    let v = versions();
    view! {
        <header node_ref=node class="c6-header hidden h-11 shrink-0 items-center gap-0.5 border-b border-zinc-200 bg-zinc-50 px-3 md:flex dark:border-zinc-800 dark:bg-zinc-950">
            <span data-header-fixed class="shrink-0"><Brand /></span>
            <MobileMenu desktop=true desktop_attention=attention />
            <span data-header-fixed class="shrink-0"><PlaceMenu /></span>
            <HeaderItem name="build" priority=0>
                {move || v.now.get().map(|full| view! {
                    <span class="rounded bg-zinc-200/70 px-1.5 py-0.5 font-mono text-[10px] text-zinc-600 dark:bg-zinc-800 dark:text-zinc-400" title=format!("carbon6 {full}")>{short_version(&full)}</span>
                })}
            </HeaderItem>
            <nav class="contents" aria-label="Main navigation">
                {if console {
                    view! {
                        <HeaderItem name="realms" priority=110><NavLink to=View::Realms label="Realms" /></HeaderItem>
                        <HeaderItem name="decisions" priority=100><decisions::DecisionLink class=NAV /></HeaderItem>
                        <HeaderItem name="workers" priority=30><NavLink to=View::Workers label="Workers" /></HeaderItem>
                        <HeaderItem name="analytics" priority=40><NavLink to=View::Analytics label="Analytics" /></HeaderItem>
                        <HeaderItem name="activity" priority=60><NavLink to=View::Activity label="Activity" /></HeaderItem>
                        <HeaderItem name="usage" priority=35><NavLink to=View::Usage label="Usage" /></HeaderItem>
                        <HeaderItem name="settings" priority=10><NavLink to=View::Settings { note: None, section: String::new() } label="Settings" /></HeaderItem>
                    }.into_any()
                } else {
                    view! {
                        <HeaderItem name="sessions" priority=110><NavLink to=View::Sessions label="Sessions" /></HeaderItem>
                        <HeaderItem name="inbox" priority=110>
                            <Go to=View::Inbox class=NAV>
                                <span title=move || { let (s, h) = counts.get(); labels::attention_title(s, h) }>
                                    "Inbox"
                                    {move || (attention.get() > 0).then(|| view! { <span data-header-count class="ml-1 rounded-full bg-amber-500 px-1.5 text-[10px] font-semibold text-white">{attention}</span> })}
                                </span>
                            </Go>
                        </HeaderItem>
                        <HeaderItem name="decisions" priority=100><decisions::DecisionLink class=NAV /></HeaderItem>
                        <HeaderItem name="projects" priority=80><projects::ProjectsLink class=NAV /></HeaderItem>
                        <HeaderItem name="work" priority=90><work::WorkLink class=NAV /></HeaderItem>
                        <HeaderItem name="docs" priority=90><docs::DocsLink class=NAV /></HeaderItem>
                        <HeaderItem name="analytics" priority=40><NavLink to=View::Analytics label="Analytics" /></HeaderItem>
                        <HeaderItem name="mission" priority=20><NavLink to=View::Mission label="Mission" /></HeaderItem>
                        <HeaderItem name="collections" priority=30><NavLink to=View::Collections { name: String::new() } label="Collections" /></HeaderItem>
                        <HeaderItem name="workers" priority=30><NavLink to=View::Workers label="Workers" /></HeaderItem>
                        <HeaderItem name="apps" priority=35><AppsMenu /></HeaderItem>
                        <HeaderItem name="settings" priority=10><NavLink to=View::Settings { note: None, section: String::new() } label="Settings" /></HeaderItem>
                    }.into_any()
                }}
            </nav>
            <span class="flex-1"></span>
            {(!console).then(|| view! { <HeaderItem name="views" priority=25><views::ViewsMenu class=NAV /></HeaderItem> })}
            <HeaderItem name="monitor" priority=60><monitor::ActivityBadge /></HeaderItem>
            <span data-header-fixed class="shrink-0"><needs::NeedsYou /></span>
            <span data-header-fixed class="shrink-0"><assistant::AskButton /></span>
            {(!console).then(|| view! { <HeaderItem name="search" priority=105><search::SearchBox /></HeaderItem> })}
            <HeaderItem name="tile" priority=15>
                <button class="rounded-md px-2 py-1 text-xs text-muted hover:bg-zinc-200/60 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100"
                    title=move || format!("Open Home starter (or reveal it): {} tiles open · ⌘1–9 switch tiles · ⌘[ ⌘] back and forward", ws.layout.with(|l| l.tiles.len()))
                    on:click=move |_| ws.new_tile()>{crate::icons::icon(crate::icons::Icon::Plus, crate::icons::Size::S14)}" Tile"</button>
            </HeaderItem>
            {(!console).then(|| view! { <span data-header-fixed class="shrink-0"><notify::NotifyToggle /></span> })}
            <HeaderItem name="connection" priority=5><ConnBadge connected=live.connected /></HeaderItem>
        </header>
    }
}

#[component]
fn HeaderItem(name: &'static str, priority: u8, children: Children) -> impl IntoView {
    view! { <span data-header-item=name data-header-priority=priority class="inline-flex shrink-0 items-center whitespace-nowrap text-sm">{children()}</span> }
}

/// Mail, Chat, Calendar and Contacts are apps: what an app feeds in this
/// realm, or it holds (the console has none).
fn feeds() -> LocalResource<Result<serde_json::Value, String>> {
    LocalResource::new(move || {
        api::realm().track();
        async move {
            if place::is_console() {
                return Err(String::new());
            }
            api::rpc("memex.feeds", serde_json::json!({})).await
        }
    })
}

/// Wide windows, where there's no ☰: the realm's mail, chat, calendar,
/// contacts, photos and memex.
#[component]
fn AppsMenu() -> impl IntoView {
    let feeds = feeds();
    let menu = NodeRef::<leptos::html::Details>::new();
    let (pop, toggled) = util::details_pop(menu);
    let close = move |_| {
        if let Some(d) = menu.get_untracked() {
            d.set_open(false);
        }
    };
    let item = "rounded-md px-2 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800";
    view! {
        <details node_ref=menu data-pop=pop on:toggle=toggled class="relative shrink-0">
            <summary class=format!("{NAV} cursor-pointer list-none whitespace-nowrap")>"Apps "{crate::icons::chevron_down()}</summary>
            <div class="c6-pop absolute left-0 top-8 z-40 flex w-48 flex-col rounded-lg border border-zinc-200 bg-white p-1 text-sm shadow-lg dark:border-zinc-700 dark:bg-zinc-900" style="--pop-origin: top left" on:click=close>
                {move || feeds.get().and_then(Result::ok).map(|f| view! {
                    {(f["mail"] == true).then(|| view! { <Go to=View::Mail class=item own=true>"Mail"</Go> })}
                    {(f["mail"] == true || f["chat"] == true).then(|| view! { <Go to=View::Brief class=item own=true>"Brief"</Go> })}
                    {(f["chat"] == true).then(|| view! { <Go to=View::Chat class=item own=true>"Chat"</Go> })}
                    {(f["calendar"] == true).then(|| view! { <Go to=View::Calendar class=item own=true>"Calendar"</Go> })}
                    {(f["contacts"] == true).then(|| view! { <Go to=View::Contacts class=item own=true>"Contacts"</Go> })}
                })}
                <Go to=View::Photos class=item own=true>"Photos"</Go>
                <Go to=View::Memex class=item own=true>"Memex"</Go>
            </div>
        </details>
    }
}

/// Waiting across every realm and the console, independent of list filters.
#[derive(Clone, Copy)]
struct PhoneWaiting(Signal<u64>);

/// Phones and narrower windows: app navigation (session search is in its picker).
#[component]
fn MobileMenu(#[prop(optional)] desktop: bool, #[prop(optional, into)] desktop_attention: Option<Signal<usize>>) -> impl IntoView {
    let ws = workspace::use_workspace();
    let attention = expect_context::<PhoneWaiting>().0;
    let connected = api::live().connected;
    let version = versions();
    let open = RwSignal::new(false);
    let drawer = NodeRef::<leptos::html::Div>::new();
    util::drawer_gesture(
        open,
        drawer,
        Callback::new(move |()| {
            open.try_set(false);
        }),
    );
    let pop = util::dismissable(
        move || open.get(),
        move || {
            open.try_set(false);
        },
    );
    let drawer_id = format!("c6-drawer-{pop}");
    let controls = drawer_id.clone();
    // A measured-away search must not take the command while inert. Open its
    // drawer and focus that live input instead, using the same platform rules
    // as every other app command. One RAF lets the open/inert effects settle.
    let keys = window_event_listener(leptos::ev::keydown, move |ev| {
        if desktop
            && ev.key().eq_ignore_ascii_case("k")
            && util::app_shortcut(&ev)
            && let Some(el) = drawer.try_get_untracked().flatten()
            && let Some(header) = el.closest("header").ok().flatten()
            && header.client_width() > 0
            && header.query_selector("[data-header-item=search][inert]").ok().flatten().is_some()
        {
            ev.prevent_default();
            open.set(true);
            request_animation_frame(move || {
                use wasm_bindgen::JsCast;
                if let Some(input) = drawer
                    .try_get_untracked()
                    .flatten()
                    .and_then(|el| el.query_selector("input[type=search]").ok().flatten())
                    .and_then(|el| el.dyn_into::<web_sys::HtmlInputElement>().ok())
                {
                    let _ = input.focus();
                    input.select();
                }
            });
        }
    });
    on_cleanup(move || keys.remove());
    let feeds = feeds();
    // Close after choosing something (not on opening the Views drop-down in it).
    let close = move |ev: web_sys::MouseEvent| {
        let target = ev.target().and_then(|t| wasm_bindgen::JsCast::dyn_into::<web_sys::Element>(t).ok());
        if target.is_some_and(|t| t.closest("summary").ok().flatten().is_some()) {
            return;
        }
        open.set(false);
    };
    view! {
        <div data-pop=pop.clone() data-header-menu=desktop.then_some("") class="relative shrink-0">
            <button data-pop=pop.clone() class="relative flex h-11 w-11 cursor-pointer items-center justify-center rounded-md text-sm text-zinc-600 md:block md:h-auto md:w-auto md:px-2 md:py-1 dark:text-zinc-400"
                aria-controls=controls aria-expanded=move || open.get().to_string() on:click=move |_| open.update(|o| *o = !*o)
                aria-describedby=desktop.then_some("c6-header-overflow-state")
                aria-label=move || format!("Menu, {} waiting across the app{}", attention.get(), if connected.get() { "" } else { ", reconnecting" })>
                {crate::icons::menu()}
                {move || (attention.get() > 0).then(|| view! { <span data-phone-waiting="true" class="absolute right-1 top-1 flex h-4 min-w-4 items-center justify-center rounded-full bg-amber-500 px-1 text-[10px] font-semibold leading-none text-white md:hidden">{attention.get()}</span> })}
                {desktop.then(|| view! {
                    <span data-header-overflow-badge aria-hidden="true" class="c6-header-overflow-badge absolute right-1 top-1 size-1.5 rounded-full bg-amber-500"></span>
                    <span id="c6-header-overflow-state" data-header-overflow-note data-header-feedback class="sr-only"></span>
                })}
                <Show when=move || !connected.get()>
                    <span class="absolute bottom-1 right-1 size-2 rounded-full bg-amber-500 md:hidden" title="Reconnecting"></span>
                </Show>
            </button>
            <div class="c6-presence c6-scrim fixed inset-0 z-30 bg-black/30 md:hidden" data-open=move || open.get().to_string() inert=move || !open.get() on:click=move |_| open.set(false)></div>
            <div node_ref=drawer id=drawer_id class="c6-sheet c6-pane c6-presence phone-drawer fixed left-0 z-40 w-72 max-w-full space-y-1 overflow-y-auto rounded-r-lg border border-zinc-200 bg-white p-2 shadow-lg md:absolute md:top-11 dark:border-zinc-700 dark:bg-zinc-900"
                data-open=move || open.get().to_string() inert=move || !open.get() on:click=close>
                <PlaceMenu drawer=true />
                {place::is_console().then(|| view! {
                    <div class="flex flex-col text-sm">
                        <Go to=View::Realms class=MENU>"Realms"</Go>
                        <span data-header-destination="decisions" class="contents"><decisions::DecisionLink class=MENU /></span>
                        <Go to=View::Workers class=MENU>"Workers"</Go>
                        <Go to=View::Analytics class=MENU>"Analytics"</Go>
                        <Go to=View::Activity class=MENU>"Activity"</Go>
                        <Go to=View::Usage class=MENU>"Usage"</Go>
                        <Go to=View::Settings { note: None, section: String::new() } class=MENU>"Settings"</Go>
                    </div>
                })}
                {desktop.then(|| view! { <div class:hidden=place::is_console() on:click=|ev| ev.stop_propagation()><search::SearchBox /></div> })}
                <div class="flex flex-col text-sm" class:hidden=place::is_console()>
                    <Go to=View::Sessions class=MENU>"Sessions"</Go>
                    <span data-header-destination="inbox" class="contents"><Go to=View::Inbox class=MENU>"Inbox"
                        {move || desktop_attention.map(|n| n.get()).filter(|n| *n > 0).map(|n| view! { <span data-header-count class="ml-1 rounded-full bg-amber-500 px-1.5 text-[10px] font-semibold text-white">{n}</span> })}
                    </Go></span>
                    <span data-header-destination="decisions" class="contents"><decisions::DecisionLink class=MENU /></span>
                    <chats::ChatLink class=MENU />
                    <span data-header-destination="projects" class="contents"><projects::ProjectsLink class=MENU /></span>
                    <span data-header-destination="work" class="contents"><work::WorkLink class=MENU /></span>
                    <docs::DocsLink class=MENU />
                    <div class="my-1 border-t border-zinc-200 dark:border-zinc-800"></div>
                    <span class="px-2 pt-1 text-xs font-medium text-muted">"Apps"</span>
                    // Only what an app feeds in this realm (or it holds).
                    {move || feeds.get().and_then(Result::ok).map(|f| view! {
                        {(f["mail"] == true).then(|| view! { <Go to=View::Mail class=MENU>"Mail"</Go> })}
                        {(f["mail"] == true || f["chat"] == true).then(|| view! { <Go to=View::Brief class=MENU>"Brief"</Go> })}
                        {(f["chat"] == true).then(|| view! { <Go to=View::Chat class=MENU>"Chat"</Go> })}
                        {(f["calendar"] == true).then(|| view! { <Go to=View::Calendar class=MENU>"Calendar"</Go> })}
                        {(f["contacts"] == true).then(|| view! { <Go to=View::Contacts class=MENU>"Contacts"</Go> })}
                    })}
                    <Go to=View::Photos class=MENU>"Photos"</Go>
                    <Go to=View::Memex class=MENU>"Memex"</Go>
                    <div class="my-1 border-t border-zinc-200 dark:border-zinc-800"></div>
                    <Go to=View::Mission class=MENU>"Mission control"</Go>
                    <Go to=View::Workers class=MENU>"Workers"</Go>
                    <Go to=View::Collections { name: String::new() } class=MENU>"Collections"</Go>
                    <views::ViewsMenu class=MENU />
                    <div class="flex items-center"><Go to=View::Activity class=MENU>"Activity"</Go><monitor::ActivityBadge /></div>
                    <Go to=View::Analytics class=MENU>"Analytics"</Go>
                    <Go to=View::Usage class=MENU>"Usage"</Go>
                    <Go to=View::Settings { note: None, section: String::new() } class=MENU>"Settings"</Go>
                </div>
                <button class=MENU on:click=move |_| ws.new_tile()>{crate::icons::icon(crate::icons::Icon::Plus, crate::icons::Size::S14)}" New tile"</button>
                <div class="space-y-2 border-t border-zinc-200 px-2 pt-2 text-xs text-muted dark:border-zinc-800">
                    <div on:click=|ev| ev.stop_propagation()><needs::NeedsYou labelled=true /></div>
                    <div on:click=|ev| ev.stop_propagation()><notify::NotifyToggle /></div>
                    <ConnBadge connected />
                    <span class="block font-mono">{move || version.now.get().map(|v| format!("carbon6 {v}"))}</span>
                </div>
            </div>
        </div>
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
        <span class="flex items-center gap-1.5 text-xs text-muted">
            <span class=move || {
                if connected.get() { "h-2 w-2 rounded-full bg-emerald-500" } else { "h-2 w-2 animate-pulse rounded-full bg-amber-500" }
            }></span>
            <span>{move || if connected.get() { "live" } else { "reconnecting…" }}</span>
        </span>
    }
}

/// A realm's mark, the same everywhere it shows (docs/marks.md).
#[component]
fn RealmGlyph(#[prop(into)] id: String) -> impl IntoView {
    let key = format!("realm:{id}");
    view! { <marks::Glyph mark=Signal::derive(move || marks::live_mark(&key)) shape=marks::Shape::Triangle size=10 /> }
}

/// Where you are (a realm, in its colour, or the console) and the way to
/// the others, each with how many things wait on you there
/// (docs/realms-ux.md §2, §6). Each is a link: ⌘-click opens it in a new
/// tab. Links to a place are `rel="external"`: a place is fixed when the
/// page loads, so the router must not handle them in the page.
#[component]
fn PlaceMenu(#[prop(optional)] drawer: bool) -> impl IntoView {
    // Loaded again as things change, like the Decisions count: an update
    // offer, say, waits in the console while you're in a realm.
    let (live, tick) = (api::live(), decisions::ticking());
    let places = LocalResource::new(move || {
        api::realms_changed().track();
        tick.track();
        live.sessions.track();
        api::elsewhere()
    });
    let menu = NodeRef::<leptos::html::Details>::new();
    let (pop, toggled) = util::details_pop(menu);
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
            Some(r) => places
                .get()
                .and_then(Result::ok)
                .and_then(|ps| ps.into_iter().find(|p| &p.id == r).map(|p| p.name))
                .unwrap_or_else(|| r.clone()),
            None => "Console".into(),
        }
    };
    let full_name = name.clone();
    view! {
        <details node_ref=menu data-pop=pop on:toggle=toggled class="relative">
            <summary class=if drawer { "flex min-h-9 cursor-pointer list-none items-center gap-1.5 rounded-md bg-zinc-100 px-2 text-sm dark:bg-zinc-800" } else { "flex max-w-[min(28rem,36vw)] cursor-pointer list-none items-center gap-1.5 rounded-md border border-zinc-300 px-2 py-0.5 text-sm dark:border-zinc-700" }
                title="Where you are: a realm, or the console">
                {match here.clone() {
                    Some(r) => view! { <RealmGlyph id=r /> }.into_any(),
                    None => view! { <span class="h-2.5 w-2.5 shrink-0 rounded-full bg-zinc-900 dark:bg-zinc-100"></span> }.into_any(),
                }}
                <span class="min-w-0 truncate font-medium" title=full_name>{name}</span>
                {move || (elsewhere() > 0).then(|| view! {
                    <span class="rounded-full bg-amber-500 px-1.5 text-[10px] font-semibold text-white" title="Waiting on you in other realms">{elsewhere()}</span>
                })}
                <span class="text-muted">{crate::icons::chevron_down()}</span>
            </summary>
            // On a phone, the width of the screen (anchored at the chip, it
            // would run off its edge).
            <div class=if drawer { "c6-pop mt-1 rounded-lg border border-zinc-200 bg-white p-1 text-sm dark:border-zinc-700 dark:bg-zinc-900" } else { "c6-pop absolute left-0 top-9 z-40 w-64 rounded-lg border border-zinc-200 bg-white p-1 text-sm shadow-lg dark:border-zinc-700 dark:bg-zinc-900" } style="--pop-origin: top left">
                <div class="px-2 py-1 text-[11px] font-semibold uppercase tracking-wide text-muted">"Realms"</div>
                {move || places.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|p| p.id != "root").map(|p| {
                    let current = here.as_deref() == Some(p.id.as_str());
                    view! {
                        <a rel="external" href=place::url_of(&place::Place::Realm(p.id.clone()))
                            class=format!("flex min-h-9 items-center gap-2 rounded-md px-2 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800 {}", if current { "bg-zinc-100 font-medium dark:bg-zinc-800" } else { "" })>
                            <RealmGlyph id=p.id.clone() />
                            <span class="min-w-0 flex-1 truncate">{p.name.clone()}</span>
                            {(p.waiting > 0).then(|| view! { <span class="rounded-full bg-amber-500 px-1.5 text-[10px] font-semibold text-white" title="Waiting on you">{p.waiting}</span> })}
                            {(current && !drawer).then(|| view! { <span class="text-muted">{crate::icons::check()}</span> })}
                        </a>
                    }
                }).collect_view()}
                <div class="my-1 border-t border-zinc-200 dark:border-zinc-800"></div>
                <a rel="external" href=place::url_of(&place::Place::Console) class="flex items-center gap-2 rounded-md px-2 py-1.5 hover:bg-zinc-100 dark:hover:bg-zinc-800">
                    <span class="h-2.5 w-2.5 shrink-0 rounded-full bg-zinc-900 dark:bg-zinc-100"></span>
                    <span class="flex-1">"Console"</span>
                    {move || console_waiting(&places).map(|n| view! { <span class="rounded-full bg-amber-500 px-1.5 text-[10px] font-semibold text-white" title="Waiting on you: installs, grants">{n}</span> })}
                    <span class="text-xs text-muted">"realms, workers, system"</span>
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
            <div class="px-2 py-1 text-[11px] font-semibold uppercase tracking-wide text-muted">"Realms"</div>
            {move || places.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|p| p.id != "root").map(|p| {
                let current = here.as_deref() == Some(p.id.as_str());
                view! {
                    <a rel="external" href=place::url_of(&place::Place::Realm(p.id.clone())) class=format!("flex items-center gap-2 {MENU} {}", if current { "font-medium" } else { "" })>
                        <RealmGlyph id=p.id.clone() />
                        <span class="min-w-0 flex-1 truncate">{p.name.clone()}</span>
                        {(p.waiting > 0).then(|| view! { <span class="rounded-full bg-amber-500 px-1.5 text-[10px] font-semibold text-white">{p.waiting}</span> })}
                        {current.then(|| view! { <span class="text-muted">{crate::icons::check()}</span> })}
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
                    <p class="text-sm text-muted">"It may have been deleted, or the address is mistyped. Go to one of yours:"</p>
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

/// The mark and name, always whole. The separate build tag yields before links.
#[component]
fn Brand() -> impl IntoView {
    view! {
        <Go to=View::Sessions class="mr-2 flex shrink-0 items-center gap-2 whitespace-nowrap">
            <span class="shrink-0" inner_html=MARK></span>
            <span class="hidden font-semibold tracking-tight sm:inline">"carbon6"</span>
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
                    <button class="rounded px-1.5 text-sky-700 hover:bg-sky-100 dark:text-sky-300 dark:hover:bg-sky-900" title="Not now" on:click=move |_| dismissed.set(Some(n2.clone()))>{crate::icons::x()}</button>
                </div>
            }
        })}
    }
}
