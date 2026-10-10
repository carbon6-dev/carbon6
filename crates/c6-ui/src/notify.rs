//! Desktop notifications when a session finishes while the tab is in the
//! background: useful with sessions running on many machines.

use std::collections::HashMap;

use c6_api::SessionStatus;
use leptos::prelude::*;
use web_sys::{Notification, NotificationOptions, NotificationPermission};

use crate::api::LiveFeed;
use crate::util::{storage_get, storage_set};

fn enabled() -> bool {
    Notification::permission() == NotificationPermission::Granted && storage_get("c6.notify").as_deref() != Some("0")
}

/// Watches the live session list and notifies on running → idle/failed.
pub fn watch_sessions(live: LiveFeed) {
    let previous = StoredValue::new(HashMap::<String, SessionStatus>::new());
    Effect::new(move || {
        let sessions = live.sessions.get();
        let hidden = document().hidden();
        previous.update_value(|prev| {
            for s in &sessions {
                let was = prev.insert(s.id.clone(), s.status);
                let finished = was == Some(SessionStatus::Running) && matches!(s.status, SessionStatus::Idle | SessionStatus::Failed);
                if finished && hidden && enabled() {
                    let opts = NotificationOptions::new();
                    opts.set_body(&format!(
                        "{} on {}",
                        if s.status == SessionStatus::Failed { "Failed" } else { "Finished" },
                        live.worker_name(&s.worker_id.to_string())
                    ));
                    opts.set_tag(&s.id);
                    if let Ok(n) = Notification::new_with_options(&s.title, &opts) {
                        let url = format!("/sessions/{}", s.id);
                        let onclick = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
                            let _ = window().focus();
                            let _ = window().location().set_href(&url);
                        });
                        n.set_onclick(Some(wasm_bindgen::JsCast::unchecked_ref(onclick.as_ref())));
                        onclick.forget();
                    }
                }
            }
        });
    });
}

#[component]
pub fn NotifyToggle() -> impl IntoView {
    let state = RwSignal::new(enabled());
    let click = move |_| {
        if state.get_untracked() {
            storage_set("c6.notify", "0");
            state.set(false);
        } else if Notification::permission() == NotificationPermission::Granted {
            storage_set("c6.notify", "1");
            state.set(true);
        } else if let Ok(p) = Notification::request_permission() {
            let done = wasm_bindgen::closure::Closure::<dyn FnMut(wasm_bindgen::JsValue)>::new(move |_| {
                storage_set("c6.notify", "1");
                state.set(enabled());
            });
            let _ = p.then(&done);
            done.forget();
        }
    };
    view! {
        <button class="text-xs text-zinc-500 hover:text-zinc-900 dark:hover:text-zinc-100" title="Notify me when a session finishes while this tab is in the background" on:click=click>
            {move || if state.get() { "🔔 on" } else { "🔕 off" }}
        </button>
    }
}
