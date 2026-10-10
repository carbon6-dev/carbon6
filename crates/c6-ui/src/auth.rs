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
        "InvalidStateError" => "This device already has a passkey for this head, probably synced from another device \
            (Google Password Manager or iCloud Keychain). You can sign in with it; no new one is needed."
            .into(),
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
            <CodeSignIn />
            <p class="text-sm text-zinc-500">
                "Or run "<code class="rounded bg-zinc-100 px-1 font-mono dark:bg-zinc-800">"c6h login-link"</code>
                " on the head and open the link it prints."
            </p>
        </div>
    }
}

/// Sign in with a one-time code made on a signed-in device.
#[component]
fn CodeSignIn() -> impl IntoView {
    let code = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let go = move || {
        let c = code.get_untracked();
        if c.trim().is_empty() || busy.get_untracked() {
            return;
        }
        busy.set(true);
        spawn_local(async move {
            match api::redeem_code(c).await {
                Ok(()) => reload_to(&window().location().pathname().unwrap_or_else(|_| "/".into())),
                Err(e) => {
                    error.set(Some(e));
                    busy.set(false);
                }
            }
        });
    };
    view! {
        <div class="space-y-2 border-t border-zinc-200 pt-4 dark:border-zinc-800">
            <label class="block text-sm text-zinc-500" for="c6-code">"Have a one-time code? Make one on a signed-in device under Settings → Sign-in."</label>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            <div class="flex gap-2">
                <input id="c6-code" class=format!("{INPUT} font-mono uppercase tracking-widest") placeholder="XXXX-XXXX"
                    autocomplete="one-time-code" autocapitalize="characters" spellcheck="false"
                    prop:value=move || code.get() on:input=move |ev| code.set(event_target_value(&ev))
                    on:keydown=move |ev| if ev.key() == "Enter" { go() } />
                <button class=format!("{BUTTON} shrink-0 whitespace-nowrap") prop:disabled=move || busy.get() on:click=move |_| go()>"Sign in"</button>
            </div>
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
pub fn SignInSettings(after_link: bool) -> impl IntoView {
    let status = LocalResource::new(api::auth_status);
    let passkeys = LocalResource::new(api::passkeys);
    let sessions = LocalResource::new(api::auth_sessions);
    let error = RwSignal::new(None::<String>);
    let name = RwSignal::new(default_name());
    let busy = RwSignal::new(false);
    let after_link = move || after_link;
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
            <p class="text-xs text-zinc-500">
                "Each passkey is listed once, named after the device it was made on. Password managers (Google Password Manager, iCloud Keychain) sync it to your other devices, which then sign in with that same passkey."
            </p>
            <div class="divide-y divide-zinc-200 rounded-lg border border-zinc-200 text-sm dark:divide-zinc-800 dark:border-zinc-800">
                {move || passkeys.get().map(|r| match r {
                    Ok(ps) if ps.is_empty() => view! { <p class="px-4 py-3 text-zinc-500">"No passkeys yet."</p> }.into_any(),
                    Ok(ps) => ps.into_iter().map(|p| {
                        let (id, rename_id) = (p.id.clone(), p.id.clone());
                        // Some(draft) while renaming.
                        let editing = RwSignal::new(None::<String>);
                        let name = p.name.clone();
                        let save = move || {
                            let Some(new) = editing.get_untracked() else { return };
                            let id = rename_id.clone();
                            spawn_local(async move {
                                match api::rename_passkey(id, new).await {
                                    Ok(()) => {
                                        editing.set(None);
                                        passkeys.refetch();
                                    }
                                    Err(e) => error.set(Some(e)),
                                }
                            });
                        };
                        let save2 = save.clone();
                        view! {
                            <div class="flex items-center justify-between gap-3 px-4 py-2">
                                <div class="min-w-0 flex-1">
                                    {move || match editing.get() {
                                        Some(draft) => {
                                            let save = save.clone();
                                            view! {
                                                <input class=format!("{INPUT} max-w-xs py-1") prop:value=draft autofocus
                                                    on:input=move |ev| editing.set(Some(event_target_value(&ev)))
                                                    on:keydown=move |ev| match ev.key().as_str() {
                                                        "Enter" => save(),
                                                        "Escape" => editing.set(None),
                                                        _ => {}
                                                    } />
                                            }.into_any()
                                        }
                                        None => view! { <div class="font-medium">{name.clone()}</div> }.into_any(),
                                    }}
                                    <div class="truncate text-xs text-zinc-500">
                                        {format!("{} · added {}", p.rp_id, ago(now_ms() - p.created_at))}
                                        {p.last_used_at.map(|t| format!(" · used {}", ago(now_ms() - t)))}
                                    </div>
                                </div>
                                {move || if editing.with(Option::is_some) {
                                    let save = save2.clone();
                                    view! {
                                        <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| save()>"Save"</button>
                                        <button class=format!("{BUTTON_QUIET} text-xs text-zinc-500") on:click=move |_| editing.set(None)>"Cancel"</button>
                                    }.into_any()
                                } else {
                                    let (id, name) = (id.clone(), p.name.clone());
                                    view! {
                                        <button class=format!("{BUTTON_QUIET} text-xs text-zinc-500") on:click=move |_| editing.set(Some(name.clone()))>"Rename"</button>
                                        <button class=format!("{BUTTON_QUIET} text-xs text-red-600") on:click=move |_| {
                                            let id = id.clone();
                                            spawn_local(async move {
                                                match api::delete_passkey(id).await {
                                                    Ok(()) => passkeys.refetch(),
                                                    Err(e) => error.set(Some(e)),
                                                }
                                            });
                                        }>"Remove"</button>
                                    }.into_any()
                                }}
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
                        "This head signs its own HTTPS certificate, so its name never appears in public certificate logs. Each device trusts it once (browsers use the system's trusted certificates): "
                        "on a Mac, open "<a class="underline" href="/ca.pem" rel="external" download="carbon6-ca.pem">"ca.pem"</a>" in Keychain Access and set Trust → Always Trust, then restart the browser; "
                        "on an iPhone or iPad, install the "<a class="underline" href="/ca.mobileconfig" rel="external" download="carbon6-ca.mobileconfig">"configuration profile"</a>
                        " and turn it on under General → About → Certificate Trust Settings; on Android, download "
                        <a class="underline" href="/ca.crt" rel="external" download="carbon6-ca.crt">"ca.crt"</a>
                        " and install it under Settings → Encryption & credentials → Install a certificate → CA certificate, then force-stop Chrome; elsewhere, add ca.pem to the system's trusted certificates. "
                        "Afterwards restart the browser fully: one that was shown the certificate warning keeps saying \"Not secure\" until it restarts. "
                        "It can only vouch for this head's names and private addresses."
                    </p>
                </div>
            })}
            <OtherDevice />
            <h3 class="pt-2 text-sm font-semibold">"Signed-in browsers"</h3>
            <div class="divide-y divide-zinc-200 rounded-lg border border-zinc-200 text-sm dark:divide-zinc-800 dark:border-zinc-800">
                {move || sessions.get().map(|r| match r {
                    Ok(ss) => ss.into_iter().map(|s| {
                        let id = s.id.clone();
                        view! {
                            <div class="flex items-center justify-between gap-3 px-4 py-2">
                                <div class="min-w-0">
                                    <div class="truncate">{browser_name(s.user_agent.as_deref())}{s.current.then_some(" (this browser)")}</div>
                                    <div class="text-xs text-zinc-500">{format!("{} · signed in {} · active {}", match s.method.as_str() { "passkey" => "passkey", "code" => "one-time code", _ => "login link" }, ago(now_ms() - s.created_at), ago(now_ms() - s.last_seen_at))}</div>
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

/// Settings: a one-time code to type in on another device.
#[component]
fn OtherDevice() -> impl IntoView {
    let code = RwSignal::new(None::<c6_api::LoginCode>);
    let error = RwSignal::new(None::<String>);
    // A 1-second clock for the countdown.
    let now = RwSignal::new(now_ms());
    let ticker = set_interval_with_handle(move || now.set(now_ms()), std::time::Duration::from_secs(1)).ok();
    on_cleanup(move || {
        if let Some(t) = ticker {
            t.clear();
        }
    });
    let create = move |_| {
        spawn_local(async move {
            match api::create_login_code().await {
                Ok(c) => {
                    error.set(None);
                    now.set(now_ms());
                    code.set(Some(c));
                }
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let origin = window().location().origin().unwrap_or_default();
    view! {
        <div class="rounded-md border border-zinc-200 px-4 py-3 text-sm dark:border-zinc-800">
            <div class="flex items-center justify-between gap-3">
                <div>
                    <div class="font-medium">"Sign in another device"</div>
                    <p class="text-zinc-500">{format!("On the other device, open {origin} and enter a one-time code.")}</p>
                </div>
                <button class=format!("{BUTTON} shrink-0") on:click=create>"Create a code"</button>
            </div>
            {move || error.get().map(|e| view! { <div class="mt-2"><ErrorBox message=e /></div> })}
            {move || code.get().map(|c| {
                let left = (c.expires_at - now.get()) / 1000;
                if left <= 0 {
                    view! { <p class="mt-2 text-zinc-500">"The code expired. Create a new one."</p> }.into_any()
                } else {
                    view! {
                        <div class="mt-3 flex items-baseline gap-3">
                            <span class="select-all font-mono text-2xl font-semibold tracking-widest">{c.code.clone()}</span>
                            <span class="text-xs text-zinc-500">{format!("works once · expires in {}:{:02}", left / 60, left % 60)}</span>
                        </div>
                    }.into_any()
                }
            })}
        </div>
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
