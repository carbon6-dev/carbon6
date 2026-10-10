//! Sign-in: the sign-in page, login links, and passkey/session settings.

use leptos::prelude::*;
use leptos::task::spawn_local;
use leptos_router::hooks::use_params_map;
use wasm_bindgen::prelude::*;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, ago, now_ms};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = c6Passkey, js_name = supported)]
    fn js_supported() -> bool;

    #[wasm_bindgen(catch, js_namespace = c6Passkey, js_name = create)]
    async fn js_create(options: &str) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(catch, js_namespace = c6Passkey, js_name = get)]
    async fn js_get(options: &str) -> Result<JsValue, JsValue>;
}

/// A browser-side WebAuthn error, readable. Cancelling the prompt is the
/// common case.
fn js_error(e: JsValue) -> String {
    let name = js_sys::Reflect::get(&e, &"name".into()).ok().and_then(|v| v.as_string()).unwrap_or_default();
    match name.as_str() {
        "NotAllowedError" => "The passkey prompt was cancelled or timed out.".into(),
        "InvalidStateError" => "This device already has a passkey for carbon6.".into(),
        _ => js_sys::Reflect::get(&e, &"message".into()).ok().and_then(|v| v.as_string()).unwrap_or_else(|| format!("{e:?}")),
    }
}

/// Runs a passkey ceremony: options from the head, the browser prompt, and
/// the browser's answer as JSON for the head.
async fn ceremony(path: &str) -> Result<serde_json::Value, String> {
    let options = api::passkey_options(path).await?.to_string();
    let answer = if path == "register" { js_create(&options).await } else { js_get(&options).await }.map_err(js_error)?;
    serde_json::from_str(&answer.as_string().unwrap_or_default()).map_err(|e| e.to_string())
}

pub async fn sign_in_with_passkey() -> Result<(), String> {
    api::passkey_login(ceremony("login").await?).await
}

fn reload_to(path: &str) {
    let _ = window().location().set_href(path);
}

const CARD: &str =
    "mx-auto mt-24 max-w-md space-y-4 rounded-xl border border-zinc-200 bg-white p-6 shadow-sm dark:border-zinc-800 dark:bg-zinc-900";

/// Shown instead of the app until the browser is signed in.
#[component]
pub fn SignIn(status: c6_api::AuthStatus) -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let passkeys = status.passkeys_here && js_supported();
    let go = move |_| {
        busy.set(true);
        spawn_local(async move {
            match sign_in_with_passkey().await {
                Ok(()) => reload_to(&window().location().pathname().unwrap_or_else(|_| "/".into())),
                Err(e) => {
                    error.set(Some(e));
                    busy.set(false);
                }
            }
        });
    };
    view! {
        <div class=CARD>
            <h1 class="text-lg font-semibold">"Sign in to carbon6"</h1>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {if passkeys {
                view! {
                    <button class=format!("{BUTTON} w-full py-2") prop:disabled=move || busy.get() on:click=go>"Sign in with a passkey"</button>
                    {(!status.has_passkeys).then(|| view! {
                        <p class="text-sm text-zinc-500">"No passkey is registered for this address yet: sign in with a login link first, then add one under Settings."</p>
                    })}
                }.into_any()
            } else {
                view! {
                    <p class="text-sm text-zinc-500">
                        "Passkeys need a secure address: the head's HTTPS name (set "<code class="font-mono">"ui_tls_head_ca"</code>
                        " in c6h.toml), or http://localhost on the head's machine."
                    </p>
                }.into_any()
            }}
            <p class="text-sm text-zinc-500">
                {if passkeys && status.has_passkeys { "No passkey on this device? " } else { "" }}
                "Run "<code class="rounded bg-zinc-100 px-1 font-mono dark:bg-zinc-800">"c6h login-link"</code>
                " on the head and open the link it prints."
            </p>
        </div>
    }
}

/// `/login/:token`: a one-time login link.
#[component]
pub fn LinkSignIn() -> impl IntoView {
    let params = use_params_map();
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let go = move |_| {
        let token = params.read_untracked().get("token").unwrap_or_default();
        busy.set(true);
        spawn_local(async move {
            match api::redeem_link(token).await {
                // Straight to adding a passkey, so the link is needed only once.
                Ok(()) => reload_to("/settings?signed_in_with_link=1#sign-in"),
                Err(e) => {
                    error.set(Some(e));
                    busy.set(false);
                }
            }
        });
    };
    view! {
        <div class=CARD>
            <h1 class="text-lg font-semibold">"Sign in this browser"</h1>
            <p class="text-sm text-zinc-500">"This one-time link signs in the browser you open it in. It stops working once used, or after 15 minutes."</p>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <button class=format!("{BUTTON} w-full py-2") prop:disabled=move || busy.get() on:click=go>"Sign in"</button>
        </div>
    }
}

/// Settings → Sign-in: passkeys, signed-in browsers, sign out.
#[component]
pub fn SignInSettings() -> impl IntoView {
    let status = LocalResource::new(api::auth_status);
    let passkeys = LocalResource::new(api::passkeys);
    let sessions = LocalResource::new(api::auth_sessions);
    let error = RwSignal::new(None::<String>);
    let name = RwSignal::new(default_name());
    let busy = RwSignal::new(false);
    let query = leptos_router::hooks::use_query_map();
    let after_link = move || query.read().get("signed_in_with_link").is_some();
    let can_add = move || status.get().and_then(Result::ok).is_some_and(|s| s.passkeys_here) && js_supported();
    let add = move |_| {
        busy.set(true);
        spawn_local(async move {
            let r = match ceremony("register").await {
                Ok(cred) => api::passkey_register(name.get_untracked(), cred).await.map(drop),
                Err(e) => Err(e),
            };
            busy.set(false);
            match r {
                Ok(()) => {
                    error.set(None);
                    passkeys.refetch();
                    status.refetch();
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let sign_out = move |_| {
        spawn_local(async move {
            let _ = api::logout().await;
            reload_to("/");
        });
    };
    view! {
        <section id="sign-in" class="space-y-3">
            <div class="flex items-start justify-between gap-3">
                <div>
                    <h2 class="text-lg font-semibold">"Sign-in"</h2>
                    <p class="text-sm text-zinc-500">"Passkeys sign this head in with Touch ID, Face ID or a security key. Lost them all? "<code class="font-mono">"c6h login-link"</code>" on the head gets you back in."</p>
                </div>
                <button class=format!("{BUTTON_QUIET} shrink-0 text-xs text-zinc-500") on:click=sign_out>"Sign out"</button>
            </div>
            {move || (after_link() && passkeys.get().and_then(Result::ok).is_some_and(|p| p.is_empty())).then(|| view! {
                <div class="rounded-md border border-sky-300 bg-sky-50 px-3 py-2 text-sm text-sky-800 dark:border-sky-900 dark:bg-sky-950 dark:text-sky-200">
                    "Signed in with a login link. Add a passkey so you can sign in without one next time."
                </div>
            })}
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <div class="divide-y divide-zinc-200 rounded-lg border border-zinc-200 text-sm dark:divide-zinc-800 dark:border-zinc-800">
                {move || passkeys.get().map(|r| match r {
                    Ok(ps) if ps.is_empty() => view! { <p class="px-4 py-3 text-zinc-500">"No passkeys yet."</p> }.into_any(),
                    Ok(ps) => ps.into_iter().map(|p| {
                        let id = p.id.clone();
                        view! {
                            <div class="flex items-center justify-between gap-3 px-4 py-2">
                                <div class="min-w-0">
                                    <div class="font-medium">{p.name.clone()}</div>
                                    <div class="truncate text-xs text-zinc-500">
                                        {format!("{} · added {}", p.rp_id, ago(now_ms() - p.created_at))}
                                        {p.last_used_at.map(|t| format!(" · used {}", ago(now_ms() - t)))}
                                    </div>
                                </div>
                                <button class=format!("{BUTTON_QUIET} text-xs text-red-600") on:click=move |_| {
                                    let id = id.clone();
                                    spawn_local(async move {
                                        match api::delete_passkey(id).await {
                                            Ok(()) => passkeys.refetch(),
                                            Err(e) => error.set(Some(e)),
                                        }
                                    });
                                }>"Remove"</button>
                            </div>
                        }
                    }).collect_view().into_any(),
                    Err(e) => view! { <div class="p-3"><ErrorBox message=e /></div> }.into_any(),
                })}
            </div>
            {move || if can_add() {
                view! {
                    <div class="flex items-center gap-2">
                        <input class=format!("{INPUT} max-w-xs") placeholder="Name, e.g. MacBook" prop:value=move || name.get() on:input=move |ev| name.set(event_target_value(&ev)) />
                        <button class=BUTTON prop:disabled=move || busy.get() on:click=add>"Add a passkey"</button>
                    </div>
                }.into_any()
            } else {
                view! {
                    <p class="text-xs text-zinc-500">"To add a passkey, open the head at its HTTPS name (or http://localhost on its machine)."</p>
                }.into_any()
            }}
            {move || status.get().and_then(Result::ok).is_some_and(|s| s.head_ca).then(|| view! {
                <div class="rounded-md border border-zinc-200 px-4 py-3 text-sm dark:border-zinc-800">
                    <div class="font-medium">"Other devices"</div>
                    <p class="mt-1 text-zinc-500">
                        "This head signs its own HTTPS certificate, so its name never appears in public certificate logs. Each device trusts it once: "
                        "on an iPhone, iPad or Mac, open the "<a class="underline" href="/ca.mobileconfig">"configuration profile"</a>
                        ", install it in Settings, and on iPhone/iPad turn it on under General → About → Certificate Trust Settings; elsewhere, add "
                        <a class="underline" href="/ca.pem">"ca.pem"</a>" to the system's trusted certificates. "
                        "It can only vouch for this head's names and private addresses."
                    </p>
                </div>
            })}
            <h3 class="pt-2 text-sm font-semibold">"Signed-in browsers"</h3>
            <div class="divide-y divide-zinc-200 rounded-lg border border-zinc-200 text-sm dark:divide-zinc-800 dark:border-zinc-800">
                {move || sessions.get().map(|r| match r {
                    Ok(ss) => ss.into_iter().map(|s| {
                        let id = s.id.clone();
                        view! {
                            <div class="flex items-center justify-between gap-3 px-4 py-2">
                                <div class="min-w-0">
                                    <div class="truncate">{browser_name(s.user_agent.as_deref())}{s.current.then_some(" (this browser)")}</div>
                                    <div class="text-xs text-zinc-500">{format!("{} · signed in {} · active {}", if s.method == "passkey" { "passkey" } else { "login link" }, ago(now_ms() - s.created_at), ago(now_ms() - s.last_seen_at))}</div>
                                </div>
                                {(!s.current).then(|| view! {
                                    <button class=format!("{BUTTON_QUIET} text-xs text-red-600") on:click=move |_| {
                                        let id = id.clone();
                                        spawn_local(async move {
                                            match api::delete_auth_session(id).await {
                                                Ok(()) => sessions.refetch(),
                                                Err(e) => error.set(Some(e)),
                                            }
                                        });
                                    }>"Sign out"</button>
                                })}
                            </div>
                        }
                    }).collect_view().into_any(),
                    Err(e) => view! { <div class="p-3"><ErrorBox message=e /></div> }.into_any(),
                })}
            </div>
        </section>
    }
}

/// A name for a new passkey from the platform, e.g. "Mac" or "iPhone".
fn default_name() -> String {
    let ua = window().navigator().user_agent().unwrap_or_default();
    let device = ["iPhone", "iPad", "Android", "Mac", "Windows", "Linux"]
        .into_iter()
        .find(|d| ua.contains(d) || (*d == "Mac" && ua.contains("Macintosh")));
    device.unwrap_or("Passkey").to_owned()
}

/// "Safari on Mac" from a user agent.
fn browser_name(ua: Option<&str>) -> String {
    let Some(ua) = ua else { return "Unknown browser".into() };
    let browser = if ua.contains("Edg/") {
        "Edge"
    } else if ua.contains("Firefox/") {
        "Firefox"
    } else if ua.contains("Chrome/") {
        "Chrome"
    } else if ua.contains("Safari/") {
        "Safari"
    } else {
        "Browser"
    };
    let os = if ua.contains("iPhone") {
        "iPhone"
    } else if ua.contains("iPad") {
        "iPad"
    } else if ua.contains("Android") {
        "Android"
    } else if ua.contains("Macintosh") {
        "Mac"
    } else if ua.contains("Windows") {
        "Windows"
    } else if ua.contains("Linux") {
        "Linux"
    } else {
        "unknown OS"
    };
    format!("{browser} on {os}")
}
