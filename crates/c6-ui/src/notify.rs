//! Desktop notifications when a session finishes while the tab is in the
//! background: useful with sessions running on many machines.

use std::collections::HashMap;

use c6_api::{Session, SessionStatus};
use leptos::prelude::*;
use web_sys::{Notification, NotificationOptions, NotificationPermission};

use crate::api::LiveFeed;
use crate::util::{storage_get, storage_set};

fn enabled() -> bool {
    Notification::permission() == NotificationPermission::Granted && storage_get("c6.notify").as_deref() != Some("0")
}

fn finished(previous: &mut HashMap<String, SessionStatus>, session: &Session) -> bool {
    let status = session.display_status();
    let was = previous.insert(session.id.clone(), status);
    was == Some(SessionStatus::Running) && matches!(status, SessionStatus::Idle | SessionStatus::Failed)
}

/// Watches the live session list and notifies on aggregated running → idle/failed,
/// for the sessions the label filter shows.
pub fn watch_sessions(live: LiveFeed, ws: crate::workspace::Workspace, filter: crate::labels::LabelFilter) {
    let previous = StoredValue::new(HashMap::<String, SessionStatus>::new());
    let waiting = StoredValue::new(Vec::<String>::new());
    Effect::new(move || {
        let sessions = live.sessions.get();
        let hidden = document().hidden();
        // Read without tracking: a filter change is not a reason to notify.
        let wanted = filter.0.get_untracked();
        previous.update_value(|prev| {
            for s in &sessions {
                let finished = finished(prev, s);
                // Starting to wait for your approval is worth a notice too.
                let asks = s.awaiting_approval && !waiting.with_value(|w| w.contains(&s.id));
                if asks {
                    waiting.update_value(|w| w.push(s.id.clone()));
                } else if !s.awaiting_approval {
                    waiting.update_value(|w| w.retain(|x| *x != s.id));
                }
                if (finished || asks) && hidden && enabled() && crate::labels::matches(s, &wanted) {
                    let opts = NotificationOptions::new();
                    opts.set_body(&format!(
                        "{} on {}",
                        if asks {
                            "Needs your approval"
                        } else if s.display_status() == SessionStatus::Failed {
                            "Failed"
                        } else {
                            "Finished"
                        },
                        live.worker_name(&s.worker_id.to_string())
                    ));
                    opts.set_tag(&s.id);
                    if let Ok(n) = Notification::new_with_options(&s.title, &opts) {
                        let id = s.id.clone();
                        let onclick = wasm_bindgen::closure::Closure::<dyn FnMut()>::new(move || {
                            let _ = window().focus();
                            ws.open(None, crate::workspace::View::session(id.clone()), crate::workspace::How::Beside);
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
    let filter = crate::labels::use_filter().0;
    let title = move || {
        let base = "Notify me when a session finishes while this tab is in the background";
        filter.with(|f| {
            if f.is_empty() { base.to_owned() } else { format!("{base} (only sessions with labels {}, as filtered)", f.join(", ")) }
        })
    };
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
        <button class="hidden text-xs text-zinc-500 hover:text-zinc-900 md:inline dark:hover:text-zinc-100" title=title on:click=click>
            {move || match (state.get(), filter.with(Vec::is_empty)) {
                (true, true) => "🔔 on",
                (true, false) => "🔔 filtered",
                _ => "🔕 off",
            }}
        </button>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        serde_json::from_value(serde_json::json!({
            "id": "s1", "worker_id": "00000000-0000-0000-0000-000000000001", "title": "t", "cwd": "/src/app",
            "connection_id": "anthropic", "model": "claude-x", "status": "idle", "error": null,
            "created_at": 0, "updated_at": 0, "last_seq": 0
        }))
        .unwrap()
    }

    #[test]
    fn completion_waits_for_side_chats_and_subagents() {
        let mut previous = HashMap::new();
        let mut s = session();
        s.status = SessionStatus::Running;
        assert!(!finished(&mut previous, &s));

        s.side_chats_running = 1;
        s.subagents_running = 1;
        s.status = SessionStatus::Idle;
        assert!(!finished(&mut previous, &s));

        s.side_chats_running = 0;
        assert!(!finished(&mut previous, &s));

        s.subagents_running = 0;
        assert!(finished(&mut previous, &s));
        assert!(!finished(&mut previous, &s));
    }

    #[test]
    fn side_chat_only_activity_can_finish_or_fail() {
        for end in [SessionStatus::Idle, SessionStatus::Failed] {
            let mut previous = HashMap::new();
            let mut s = session();
            assert!(!finished(&mut previous, &s));
            s.side_chats_running = 1;
            assert!(!finished(&mut previous, &s));
            s.side_chats_running = 0;
            s.status = end;
            assert!(finished(&mut previous, &s));
            assert!(!finished(&mut previous, &s));
        }
    }
}
