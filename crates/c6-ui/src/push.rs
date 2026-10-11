//! Web push on this device, and the devices that get notifications.

use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::prelude::*;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, ago, now_ms};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = c6Push, js_name = state)]
    fn js_state() -> String;

    #[wasm_bindgen(catch, js_namespace = c6Push, js_name = current)]
    async fn js_current() -> Result<JsValue, JsValue>;

    #[wasm_bindgen(catch, js_namespace = c6Push, js_name = enable)]
    async fn js_enable(key: &str) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(catch, js_namespace = c6Push, js_name = disable)]
    async fn js_disable() -> Result<JsValue, JsValue>;
}

fn js_error(e: JsValue) -> String {
    e.dyn_ref::<js_sys::Error>().map(|e| String::from(e.message())).or_else(|| e.as_string()).unwrap_or_else(|| "failed".into())
}

/// A name for this device, from its browser.
fn device_name() -> String {
    let ua = window().navigator().user_agent().unwrap_or_default();
    let os = ["iPhone", "iPad", "Android", "Mac OS X", "Windows", "Linux"].into_iter().find(|o| ua.contains(o)).unwrap_or("");
    let os = if os == "Mac OS X" { "Mac" } else { os };
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
    if os.is_empty() { browser.to_owned() } else { format!("{browser} on {os}") }
}

/// The endpoint of a subscription's JSON.
fn endpoint_of(json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(json).ok().and_then(|v| v["endpoint"].as_str().map(str::to_owned)).unwrap_or_default()
}

#[component]
pub fn PushSetting() -> impl IntoView {
    let devices = LocalResource::new(api::push_devices);
    let state = RwSignal::new(js_state());
    // This device's endpoint, if it is subscribed.
    let mine = RwSignal::new(String::new());
    let status = RwSignal::new(None::<Result<String, String>>);
    spawn_local(async move {
        if let Ok(v) = js_current().await {
            mine.set(endpoint_of(&v.as_string().unwrap_or_default()));
        }
    });
    let enable = move |_| {
        status.set(Some(Ok("Asking…".into())));
        spawn_local(async move {
            let r = async {
                let key = api::push_key().await?.key;
                let json = js_enable(&key).await.map_err(js_error)?.as_string().unwrap_or_default();
                let mut sub: c6_api::PushSubscription = serde_json::from_str(&json).map_err(|e| e.to_string())?;
                sub.name = device_name();
                mine.set(sub.endpoint.clone());
                api::push_subscribe(sub).await
            }
            .await;
            state.set(js_state());
            status.set(Some(r.map(|d| format!("{} gets notifications now. Add a channel of kind push to choose which.", d.name))));
            devices.refetch();
        });
    };
    let disable = move |id: Option<String>| {
        spawn_local(async move {
            let r = async {
                if let Some(id) = id {
                    return api::push_unsubscribe(id).await;
                }
                // This device: unsubscribe here, then forget it on the head.
                let endpoint = js_disable().await.map_err(js_error)?.as_string().unwrap_or_default();
                mine.set(String::new());
                match api::push_devices().await?.into_iter().find(|d| d.endpoint == endpoint) {
                    Some(d) => api::push_unsubscribe(d.id).await,
                    None => Ok(()),
                }
            }
            .await;
            if let Err(e) = r {
                status.set(Some(Err(e)));
            }
            devices.refetch();
        });
    };
    view! {
        <section class="-mt-3 space-y-2 rounded-lg border border-zinc-200 p-4 text-sm dark:border-zinc-800">
            <div class="flex flex-wrap items-center gap-2">
                <span class="font-medium">"Notifications on this device"</span>
                <span class="flex-1"></span>
                {move || match state.get().as_str() {
                    "unsupported" => view! {
                        <span class="text-xs text-zinc-500">"Not available here: it needs HTTPS (or localhost). On iPhone and iPad, add carbon6 to the Home Screen (Share → Add to Home Screen) and open it from there."</span>
                    }.into_any(),
                    "denied" => view! { <span class="text-xs text-red-600">"Blocked in this browser's site settings."</span> }.into_any(),
                    _ if !mine.get().is_empty() => view! { <button class=BUTTON_QUIET on:click=move |_| disable(None)>"Turn off here"</button> }.into_any(),
                    _ => view! { <button class=BUTTON on:click=enable>"Turn on here"</button> }.into_any(),
                }}
            </div>
            <p class="text-xs text-zinc-500">"Then add a channel of kind push above: those events reach every device turned on here, and tapping an approval opens its Allow / Deny page."</p>
            {move || status.get().map(|r| match r {
                Ok(m) => view! { <p class="text-xs text-emerald-600">{m}</p> }.into_any(),
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
            })}
            {move || devices.get().and_then(Result::ok).filter(|d| !d.is_empty()).map(|ds| view! {
                <div class="divide-y divide-zinc-100 text-xs dark:divide-zinc-800">
                    {ds.into_iter().map(|d| {
                        let id = d.id.clone();
                        let this = d.endpoint == mine.get_untracked();
                        let sent = d.last_sent_at.map(|t| format!(" · last sent {} ago", ago(now_ms() - t))).unwrap_or_default();
                        view! {
                            <div class="flex flex-wrap items-center gap-2 py-1">
                                <span>{d.name.clone()}</span>
                                {this.then(|| view! { <span class="rounded bg-sky-100 px-1 text-sky-800 dark:bg-sky-950 dark:text-sky-300">"this device"</span> })}
                                <span class="text-zinc-500">{format!("via {}{sent}", d.service)}</span>
                                {d.last_error.clone().map(|e| view! { <span class="text-red-600">{e}</span> })}
                                <span class="flex-1"></span>
                                <button class="text-red-600 hover:underline" on:click=move |_| disable(Some(id.clone()))>"Remove"</button>
                            </div>
                        }
                    }).collect_view()}
                </div>
            })}
        </section>
    }
}
