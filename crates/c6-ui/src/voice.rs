//! A recording is a draft edit, never a send. Audio and upload live in voice.js.
use leptos::prelude::*;
use leptos::task::spawn_local;
use send_wrapper::SendWrapper;
use wasm_bindgen::{JsCast, prelude::*};

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = c6Voice, js_name = create, catch)]
    fn create(state: &js_sys::Function, text: &js_sys::Function, options: &JsValue) -> Result<JsValue, JsValue>;
}

#[derive(Clone, Default, serde::Deserialize)]
struct State {
    state: String,
    #[serde(default)]
    elapsed: f64,
    error: Option<String>,
    #[serde(default)]
    retry: bool,
}

struct Bridge {
    controller: JsValue,
    // Keep callbacks alive until dispose has stopped JS from calling them.
    _state: Closure<dyn Fn(String)>,
    _text: Closure<dyn Fn(String)>,
}

fn invoke(controller: &JsValue, method: &str) -> Result<(), JsValue> {
    js_sys::Reflect::get(controller, &method.into())?.dyn_into::<js_sys::Function>()?.call0(controller).map(|_| ())
}

/// DOM selections count UTF-16 units, not UTF-8 bytes (or scalar values).
fn byte_at(text: &str, utf16: u32) -> usize {
    let mut units = 0;
    for (byte, c) in text.char_indices() {
        if units + c.len_utf16() > utf16 as usize {
            return byte;
        }
        units += c.len_utf16();
    }
    text.len()
}

#[derive(Clone, Debug)]
struct Insertion {
    text: String,
    start: usize,
    end: usize,
}

fn insert(text: &str, selection: Option<(u32, u32)>, transcript: &str) -> Insertion {
    let (start, end) = selection.map(|(s, e)| (byte_at(text, s), byte_at(text, e))).unwrap_or((text.len(), text.len()));
    let end = end.max(start);
    let left = &text[..start];
    let right = &text[end..];
    let prefix =
        if !left.is_empty() && !left.ends_with(char::is_whitespace) && !transcript.starts_with(char::is_whitespace) { " " } else { "" };
    let suffix =
        if !right.is_empty() && !right.starts_with(char::is_whitespace) && !transcript.ends_with(char::is_whitespace) { " " } else { "" };
    let start = left.len() + prefix.len();
    let end = start + transcript.len();
    Insertion { text: format!("{left}{prefix}{transcript}{suffix}{right}"), start, end }
}

/// Only replace the exact version that Tidy/Undo were offered for: a delayed
/// response must never overwrite typing, a sent draft, or another recording.
fn replace_if_unchanged(current: &str, expected: &str, replacement: &str) -> Option<String> {
    (current == expected).then(|| replacement.to_owned())
}

fn selection(area: NodeRef<leptos::html::Textarea>) -> Option<(u32, u32)> {
    let el = area.get_untracked()?;
    Some((el.selection_start().ok()??, el.selection_end().ok()??))
}

fn put(area: NodeRef<leptos::html::Textarea>, text: RwSignal<String>, value: String, end: usize) {
    let caret = value[..end].encode_utf16().count() as u32;
    text.set(value.clone());
    if let Some(el) = area.get_untracked() {
        // Update the DOM before moving the caret; the signal's render is deferred.
        el.set_value(&value);
        let _ = el.focus();
        let _ = el.set_selection_range(caret, caret);
        // Completion and other caret consumers use the same events as typing.
        if let Ok(event) = web_sys::Event::new("input") {
            let _ = el.dispatch_event(&event);
        }
    }
}

#[component]
pub(crate) fn Voice(
    text: RwSignal<String>,
    area: NodeRef<leptos::html::Textarea>,
    #[prop(optional)] session_id: Option<String>,
    #[prop(optional, into)] realm: Option<Signal<String>>,
    #[prop(optional, into)] disabled: Signal<bool>,
) -> impl IntoView {
    let session_id = StoredValue::new(session_id);
    let state = RwSignal::new(State::default());
    let bridge = StoredValue::new(None::<SendWrapper<Bridge>>);
    let captured = StoredValue::new(None::<(String, Option<(u32, u32)>)>);
    let inserted = RwSignal::new(None::<Insertion>);
    let undo = RwSignal::new(None::<(String, String)>);
    let tidying = RwSignal::new(false);
    let tidy_error = RwSignal::new(None::<String>);
    let report = move |message: String| state.set(State { state: "error".into(), error: Some(message), ..State::default() });
    let command = move |method: &str| {
        bridge.with_value(|b| {
            if let Some(b) = b
                && let Err(e) = invoke(&b.controller, method)
            {
                report(e.as_string().unwrap_or_else(|| "Voice recording failed. Try again or reload the page.".into()));
            }
        });
    };
    let start = move |_| {
        if state.with_untracked(|s| s.state == "recording") {
            command("stop");
            return;
        }
        captured.set_value(Some((text.get_untracked(), selection(area))));
        inserted.set(None);
        undo.set(None);
        tidy_error.set(None);
        {
            bridge.update_value(|b| {
                if let Some(b) = b.take() {
                    let _ = invoke(&b.controller, "dispose");
                }
            });
            let on_state = Closure::<dyn Fn(String)>::new(move |json: String| {
                if let Ok(value) = serde_json::from_str(&json) {
                    state.try_set(value);
                }
            });
            let on_text = Closure::<dyn Fn(String)>::new(move |transcript: String| {
                if transcript.trim().is_empty() {
                    return;
                }
                let current = text.get_untracked();
                let at = captured
                    .with_value(|c| c.as_ref().filter(|(before, _)| before == &current).and_then(|(_, at)| *at))
                    .or_else(|| selection(area));
                let edit = insert(&current, at, &transcript);
                put(area, text, edit.text.clone(), edit.end);
                inserted.set(Some(edit));
            });
            let path = session_id.with_value(|id| match id {
                Some(id) => format!("/api/v1/transcribe?session_id={}", crate::api::enc(id)),
                None => "/api/v1/transcribe".into(),
            });
            let headers = js_sys::Object::new();
            if let Some(realm) = realm.map(|r| r.get_untracked()).or_else(crate::place::header) {
                let _ = js_sys::Reflect::set(&headers, &"X-C6-Realm".into(), &realm.into());
            }
            let options = js_sys::Object::new();
            let _ = js_sys::Reflect::set(&options, &"url".into(), &path.into());
            let _ = js_sys::Reflect::set(&options, &"headers".into(), &headers);
            match create(on_state.as_ref().unchecked_ref(), on_text.as_ref().unchecked_ref(), &options) {
                Ok(controller) => bridge.set_value(Some(SendWrapper::new(Bridge { controller, _state: on_state, _text: on_text }))),
                Err(_) => {
                    report("Voice recording is unavailable. Reload the page and use HTTPS or localhost.".into());
                    return;
                }
            }
        }
        command("start");
    };
    on_cleanup(move || {
        bridge.update_value(|b| {
            if let Some(b) = b.take() {
                let _ = invoke(&b.controller, "dispose");
            }
        });
    });
    let tidy = move |_| {
        let Some(edit) = inserted.get_untracked() else { return };
        if text.get_untracked() != edit.text {
            return;
        }
        let raw = edit.text[edit.start..edit.end].to_owned();
        let id = session_id.get_value();
        let realm = realm.map(|r| r.get_untracked());
        tidying.set(true);
        tidy_error.set(None);
        spawn_local(async move {
            let result = crate::api::voice_tidy(raw, id, realm).await;
            if tidying.try_get_untracked().is_none() {
                return;
            }
            match result {
                Ok(tidy) => {
                    let next = format!("{}{}{}", &edit.text[..edit.start], tidy, &edit.text[edit.end..]);
                    if let Some(value) = text.try_get_untracked().and_then(|current| replace_if_unchanged(&current, &edit.text, &next)) {
                        undo.set(Some((value.clone(), edit.text)));
                        inserted.set(None);
                        put(area, text, value, edit.start + tidy.len());
                    }
                }
                Err(e) => {
                    tidy_error.try_set(Some(e));
                }
            }
            tidying.try_set(false);
        });
    };
    let tool = "inline-flex min-h-8 items-center justify-center gap-1 rounded-md px-1.5 text-xs text-muted hover:bg-zinc-100 dark:hover:bg-zinc-800 disabled:opacity-40 pointer-coarse:min-h-11 pointer-coarse:min-w-11";
    view! {
        <div class="inline-flex min-w-0 max-w-full flex-wrap items-center gap-1" aria-label="Voice dictation">
            <button type="button" class=tool aria-label=move || if state.with(|s| s.state == "recording") { "Stop dictation" } else { "Dictate prompt" }
                title="Dictate a prompt; review before sending" disabled=move || state.with(|s| s.state != "recording") && (disabled.get() || tidying.get() || state.with(|s| matches!(s.state.as_str(), "requesting" | "transcribing")))
                on:mousedown=|ev| ev.prevent_default() on:click=start>
                {crate::icons::microphone()}
                {move || state.with(|s| (s.state == "recording").then(|| view! { <span class="flex items-center gap-1 text-red-600"><span class="size-2 animate-pulse rounded-full bg-red-600"></span>{format!("{}:{:02}", s.elapsed as u64 / 60, s.elapsed as u64 % 60)}</span> }))}
            </button>
            <span role="status" aria-live="polite" class="inline-flex items-center gap-1 text-xs text-muted">
                {move || state.with(|s| s.state == "transcribing").then(|| view! { <span class="inline-flex animate-spin" aria-hidden="true">{crate::icons::loader_circle()}</span> })}
                {move || state.with(|s| match s.state.as_str() { "requesting" => "Requesting microphone…", "transcribing" => "Transcribing…", _ => "" })}
            </span>
            {move || state.with(|s| matches!(s.state.as_str(), "requesting" | "recording" | "transcribing")).then(|| view! { <button type="button" class=tool on:click=move |_| command("cancel")>"Cancel"</button> })}
            {move || state.with(|s| s.error.clone()).map(|e| view! { <span role="alert" class="max-w-64 break-words text-xs text-red-600">{e}</span> })}
            {move || state.with(|s| s.state == "error" && s.retry).then(|| view! { <button type="button" class=tool on:click=move |_| command("retry")>"Retry"</button> })}
            {move || inserted.with(|e| e.as_ref().is_some_and(|e| e.text == text.get())).then(|| view! {
                <button type="button" class=tool disabled=move || tidying.get() on:click=tidy>{move || if tidying.get() { "Tidying…" } else { "Tidy" }}</button>
            })}
            {move || undo.with(|u| u.as_ref().is_some_and(|(expected, _)| *expected == text.get())).then(|| view! {
                <button type="button" class=tool on:click=move |_| {
                    if let Some((expected, raw)) = undo.get_untracked() && let Some(value) = replace_if_unchanged(&text.get_untracked(), &expected, &raw) {
                        let end = value.len();
                        put(area, text, value, end);
                        undo.set(None);
                    }
                }>"Undo tidy"</button>
            })}
            {move || tidy_error.get().map(|e| view! { <span role="alert" class="text-xs text-red-600">{e}</span> })}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caret_insertion_and_selection() {
        assert_eq!(insert("hello world", Some((6, 6)), "brave").text, "hello brave world");
        assert_eq!(insert("hello world", Some((6, 11)), "you").text, "hello you");
    }

    #[test]
    fn append_and_repeated_dictation() {
        assert_eq!(insert("hello", None, "world").text, "hello world");
        assert_eq!(insert("hello ", None, "world").text, "hello world");
        assert_eq!(insert("", None, "hello").text, "hello");
        let first = insert("", None, "hello");
        let second = insert(&first.text, Some((first.end as u32, first.end as u32)), "world");
        let third = insert(&second.text, None, "again");
        assert_eq!(third.text, "hello world again");
        assert_eq!(&third.text[third.start..third.end], "again");
    }

    #[test]
    fn unicode_caret_uses_utf16() {
        let edit = insert("a😀é z", Some((3, 4)), "日本語");
        assert_eq!(edit.text, "a😀 日本語 z");
        assert_eq!(&edit.text[edit.start..edit.end], "日本語");
        assert_eq!(byte_at("😀a", 1), 0, "do not split a surrogate pair");
    }

    #[test]
    fn tidy_and_undo_never_overwrite_edits() {
        assert_eq!(replace_if_unchanged("um hello", "um hello", "Hello."), Some("Hello.".into()));
        assert_eq!(replace_if_unchanged("Hello.", "Hello.", "um hello"), Some("um hello".into()));
        assert_eq!(replace_if_unchanged("Hello. More typing", "Hello.", "um hello"), None);
        assert_eq!(replace_if_unchanged("", "um hello", "Hello."), None, "a sent/cleared composer stays empty");
    }
}
