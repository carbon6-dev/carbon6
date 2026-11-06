//! Desktop notifications when a session finishes while the tab is in the
//! background: useful with sessions running on many machines.

use std::collections::HashMap;

use c6_api::{Session, SessionStatus};
use leptos::prelude::*;
use web_sys::{Notification, NotificationOptions, NotificationPermission};

use crate::api::LiveFeed;
use crate::icons::{Icon, Size, icon};
use crate::util::{storage_get, storage_set};

/// Some mobile browsers have no Notification global at all. Reflective
/// reads also catch permission getters that are unavailable or throw.
fn notification_api() -> Option<wasm_bindgen::JsValue> {
    #[cfg(target_arch = "wasm32")]
    {
        let window = web_sys::window()?;
        js_sys::Reflect::get(&window, &"Notification".into()).ok().filter(wasm_bindgen::JsValue::is_function)
    }
    #[cfg(not(target_arch = "wasm32"))]
    None
}

fn permission() -> Option<NotificationPermission> {
    let api = notification_api()?;
    match js_sys::Reflect::get(&api, &"permission".into()).ok()?.as_string()?.as_str() {
        "granted" => Some(NotificationPermission::Granted),
        "denied" => Some(NotificationPermission::Denied),
        "default" => Some(NotificationPermission::Default),
        _ => None,
    }
}

fn request_permission() -> Option<js_sys::Promise> {
    let api = notification_api()?;
    js_sys::Reflect::get(&api, &"requestPermission".into()).ok().filter(wasm_bindgen::JsValue::is_function)?;
    Notification::request_permission().ok()
}

fn enabled() -> bool {
    permission() == Some(NotificationPermission::Granted) && storage_get("c6.notify").as_deref() != Some("0")
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
                        live.session_worker(s)
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

fn toggle_state(on: bool, filtered: bool) -> (Icon, &'static str) {
    match (on, filtered) {
        (true, false) => (Icon::Bell, "Notifications on"),
        (true, true) => (Icon::BellDot, "Notifications filtered"),
        _ => (Icon::BellOff, "Notifications off"),
    }
}

fn toggle_title(on: bool, labels: &[String], available: bool) -> String {
    let (_, label) = toggle_state(on, !labels.is_empty());
    if !available {
        return format!("{label}. Browser notifications are not available here");
    }
    let base = "Notify me when a session finishes while this tab is in the background";
    if labels.is_empty() {
        format!("{label}. {base}")
    } else {
        format!("{label}. {base} (only sessions with labels {}, as filtered)", labels.join(", "))
    }
}

#[component]
pub fn NotifyToggle() -> impl IntoView {
    let available = permission().is_some();
    let state = RwSignal::new(enabled());
    let filter = crate::labels::use_filter().0;
    let title = move || filter.with(|f| toggle_title(state.get(), f, available));
    let click = move |_| {
        if state.get_untracked() {
            storage_set("c6.notify", "0");
            state.set(false);
        } else if permission() == Some(NotificationPermission::Granted) {
            storage_set("c6.notify", "1");
            state.set(true);
        } else if let Some(p) = request_permission() {
            leptos::task::spawn_local(async move {
                if wasm_bindgen_futures::JsFuture::from(p).await.is_ok() {
                    storage_set("c6.notify", "1");
                    let _ = state.try_set(enabled());
                }
            });
        }
    };
    view! {
        <button class="hidden text-xs text-muted hover:text-zinc-900 md:inline dark:hover:text-zinc-100" title=title aria-label=title disabled=!available on:click=click>
            {move || icon(toggle_state(state.get(), !filter.with(Vec::is_empty)).0, Size::S16)}
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

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn missing_browser_notifications_leave_the_toggle_renderable_and_off() {
        assert!(permission().is_none());
        assert!(request_permission().is_none());
        assert!(!enabled());
        let owner = Owner::new();
        owner.with(|| {
            provide_context(crate::labels::LabelFilter(RwSignal::new(Vec::new())));
            let html = view! { <NotifyToggle /> }.to_html();
            let label = "Notifications off. Browser notifications are not available here";
            assert!(html.contains(&format!("title=\"{label}\"")), "{html}");
            assert!(html.contains(&format!("aria-label=\"{label}\"")), "{html}");
            assert!(!html.split_once('>').unwrap().1.contains("Notifications off"), "the bell stands alone: {html}");
            assert_eq!(html.matches("<svg").count(), 1, "keep the notification bell: {html}");
            assert!(html.contains("disabled"), "{html}");
            assert!(html.contains("Browser notifications are not available here"), "{html}");
        });
    }

    #[test]
    fn notification_states_keep_distinct_icons_and_accessible_details() {
        assert_eq!(toggle_state(true, false), (Icon::Bell, "Notifications on"));
        assert_eq!(toggle_state(true, true), (Icon::BellDot, "Notifications filtered"));
        for filtered in [true, false] {
            assert_eq!(toggle_state(false, filtered), (Icon::BellOff, "Notifications off"));
        }
        for on in [true, false] {
            for labels in [vec![], vec!["repo:carbon6".to_owned(), "worker:home".to_owned()]] {
                let title = toggle_title(on, &labels, true);
                assert!(title.starts_with(toggle_state(on, !labels.is_empty()).1), "{title}");
                assert!(title.contains("while this tab is in the background"), "{title}");
                if !labels.is_empty() {
                    assert!(title.contains("only sessions with labels repo:carbon6, worker:home, as filtered"), "{title}");
                }
            }
        }
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
