//! The box to post (docs/chat.md §3): a formatting bar, completion for
//! @people, #channels and :emoji:, a draft kept per conversation, ↑ to
//! edit your last message, "also send to the channel" for a thread reply,
//! and a few seconds to take a post back.

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;

use super::{Chat, Place, text};
use crate::api;
use crate::util::{ErrorBox, storage_get, storage_set};

/// Seconds a post can be taken back before it goes.
const UNDO_SECONDS: u64 = 5;

/// @Name and #channel as Slack's markup (`<@U1>`, `<#C1>`), for the
/// people and channels of the workspace.
pub(crate) fn encode(text: &str, workspace: &str, chat: Chat) -> String {
    let people: Vec<(String, String)> = chat.people.with_untracked(|p| {
        p.iter()
            .filter(|x| x["workspace"] == workspace)
            .filter_map(|x| Some((x["name"].as_str()?.to_owned(), x["id"].as_str()?.to_owned())))
            .collect()
    });
    let channels = chat.known(workspace).channels;
    let mut out = String::new();
    let mut i = 0;
    while i < text.len() {
        let rest = &text[i..];
        let c = rest.chars().next().unwrap();
        let prev = text[..i].chars().next_back();
        let edge = prev.is_none_or(|p| p.is_whitespace() || "([{\"'".contains(p));
        let found = |list: &[(String, String)]| {
            list.iter()
                .filter(|(n, _)| {
                    !n.is_empty()
                        && rest.len() > n.len()
                        && rest.is_char_boundary(1 + n.len())
                        && rest[1..1 + n.len()].eq_ignore_ascii_case(n)
                        && rest[1 + n.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric())
                })
                .max_by_key(|(n, _)| n.len())
                .cloned()
        };
        if edge && c == '@' {
            if let Some(s) = ["here", "channel", "everyone"].iter().find(|s| rest[1..].starts_with(**s)) {
                out.push_str(&format!("<!{s}>"));
                i += 1 + s.len();
                continue;
            }
            if let Some((n, id)) = found(&people) {
                out.push_str(&format!("<@{id}>"));
                i += 1 + n.len();
                continue;
            }
        }
        if edge
            && c == '#'
            && let Some((n, id)) = found(&channels)
        {
            out.push_str(&format!("<#{id}>"));
            i += 1 + n.len();
            continue;
        }
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// What's being completed: the trigger, the word so far, where it starts.
fn completing(text: &str, caret: usize) -> Option<(char, String, usize)> {
    let before = text.get(..caret)?;
    let start = before.rfind(|c: char| c.is_whitespace()).map_or(0, |i| i + 1);
    let word = &before[start..];
    let t = word.chars().next()?;
    let rest = &word[t.len_utf8()..];
    match t {
        '@' | '#' if !rest.contains(['@', '#']) => Some((t, rest.to_lowercase(), start)),
        ':' if rest.len() >= 2 && !rest.contains(':') => Some((t, rest.to_lowercase(), start)),
        _ => None,
    }
}

#[component]
pub(crate) fn Composer(place: Place, thread_ts: Option<String>) -> impl IntoView {
    let chat = Chat::get();
    let key = format!("c6.chat.draft.{}.{}.{}", place.workspace, place.channel, thread_ts.clone().unwrap_or_default());
    // Its reply box (docs/conversations.md §3).
    let target = StoredValue::new(match &thread_ts {
        Some(t) => format!("chat:{}:{}:{t}", place.workspace, place.channel),
        None => format!("chat:{}:{}", place.workspace, place.channel),
    });
    let text = RwSignal::new(storage_get(&key).unwrap_or_default());
    let pending = RwSignal::new(None::<(String, u32)>);
    let error = RwSignal::new(None::<String>);
    let broadcast = RwSignal::new(false);
    let area = NodeRef::<leptos::html::Textarea>::new();
    let caret = RwSignal::new(0usize);
    let picked = RwSignal::new(0usize);
    let in_thread = thread_ts.is_some();
    // Kept as you type.
    let k2 = key.clone();
    Effect::new(move || storage_set(&k2, &text.get()));
    let ws = place.workspace.clone();
    let options = Memo::new(move |_| {
        let (t, word, _) = completing(&text.get(), caret.get())?;
        let list: Vec<(String, String, String)> = match t {
            '@' => {
                let mut l: Vec<(String, String, String)> = chat.people.with(|p| {
                    p.iter()
                        .filter(|x| x["workspace"] == ws.as_str() && x["name"].as_str().is_some_and(|n| n.to_lowercase().contains(&word)))
                        .take(8)
                        .map(|x| {
                            let n = x["name"].as_str().unwrap_or("").to_owned();
                            (format!("@{n} "), n, x["title"].as_str().unwrap_or("").to_owned())
                        })
                        .collect()
                });
                for s in ["here", "channel"] {
                    if s.starts_with(&word) {
                        l.push((format!("@{s} "), format!("@{s}"), "Notify everyone here".into()));
                    }
                }
                l
            }
            '#' => chat
                .known(&ws)
                .channels
                .into_iter()
                .filter(|(n, _)| n.to_lowercase().contains(&word))
                .take(8)
                .map(|(n, _)| (format!("#{n} "), format!("#{n}"), String::new()))
                .collect(),
            _ => {
                let mut seen = std::collections::HashSet::new();
                text::EMOJI
                    .iter()
                    .filter(|(n, e)| n.contains(word.as_str()) && seen.insert(*e))
                    .take(8)
                    .map(|(n, e)| (format!(":{n}: "), format!("{e} :{n}:"), String::new()))
                    .collect()
            }
        };
        (!list.is_empty()).then_some(list)
    });
    let complete = move |insert: String| {
        let t = text.get_untracked();
        let c = caret.get_untracked();
        let Some((_, _, start)) = completing(&t, c) else { return };
        let next = format!("{}{insert}{}", &t[..start], &t[c..]);
        let at = start + insert.len();
        text.set(next);
        caret.set(at);
        picked.set(0);
        if let Some(el) = area.get_untracked() {
            let _ = el.focus();
            set_timeout(
                move || {
                    if let Some(el) = area.get_untracked() {
                        let _ = el.set_selection_range(at as u32, at as u32);
                    }
                },
                std::time::Duration::from_millis(0),
            );
        }
    };
    // Wraps the selection in a mark (or adds it at the caret).
    let wrap = move |left: &'static str, right: &'static str| {
        let Some(el) = area.get_untracked() else { return };
        let t = text.get_untracked();
        let (a, b) = (el.selection_start().ok().flatten().unwrap_or(0) as usize, el.selection_end().ok().flatten().unwrap_or(0) as usize);
        let (a, b) = (a.min(t.len()), b.min(t.len()));
        if !t.is_char_boundary(a) || !t.is_char_boundary(b) {
            return;
        }
        text.set(format!("{}{left}{}{right}{}", &t[..a], &t[a..b], &t[b..]));
        let _ = el.focus();
        let (s, e) = ((a + left.len()) as u32, (b + left.len()) as u32);
        set_timeout(
            move || {
                if let Some(el) = area.get_untracked() {
                    let _ = el.set_selection_range(s, e);
                }
            },
            std::time::Duration::from_millis(0),
        );
    };
    let line = move |prefix: &'static str| {
        let t = text.get_untracked();
        let c = caret.get_untracked().min(t.len());
        let start = t[..c].rfind('\n').map_or(0, |i| i + 1);
        text.set(format!("{}{prefix}{}", &t[..start], &t[start..]));
    };
    let send = {
        let (place, thread_ts) = (place.clone(), thread_ts.clone());
        move || {
            let t = text.get_untracked();
            if t.trim().is_empty() || pending.get_untracked().is_some() {
                return;
            }
            text.set(String::new());
            crate::compose::forget(target.get_value());
            let (place, thread_ts) = (place.clone(), thread_ts.clone());
            let id = js_sys::Math::random().to_bits() as u32;
            let also = broadcast.get_untracked();
            pending.set(Some((t.clone(), id)));
            spawn_local(async move {
                gloo_timers::future::sleep(std::time::Duration::from_secs(UNDO_SECONDS)).await;
                // Taken back (or replaced) meanwhile: nothing goes.
                if pending.get_untracked().map(|(_, i)| i) != Some(id) {
                    return;
                }
                pending.set(None);
                let mut params =
                    json!({"workspace": place.workspace, "channel": place.channel, "text": encode(&t, &place.workspace, chat)});
                if let Some(ts) = thread_ts {
                    params["thread_ts"] = json!(ts);
                    params["broadcast"] = json!(also);
                }
                match api::rpc("memex.chat.post", params).await {
                    Ok(_) => {
                        error.set(None);
                        broadcast.set(false);
                        chat.refresh();
                    }
                    Err(e) => {
                        error.set(Some(e));
                        text.set(t);
                    }
                }
            });
        }
    };
    // ↑ in an empty box: your last message here, to edit.
    let edit_last = {
        let (place, thread_ts) = (place.clone(), thread_ts.clone());
        move || {
            let (place, thread_ts) = (place.clone(), thread_ts.clone());
            spawn_local(async move {
                let v = match &thread_ts {
                    Some(t) => {
                        api::rpc("memex.chat.thread", json!({"workspace": place.workspace, "channel": place.channel, "thread_ts": t})).await
                    }
                    None => {
                        api::rpc("memex.chat.messages", json!({"workspace": place.workspace, "channel": place.channel, "limit": 30})).await
                    }
                };
                let Ok(v) = v else { return };
                let list = v.as_array().cloned().unwrap_or_default();
                let mine = |m: &&serde_json::Value| m["meta"]["from_me"] == true;
                let last = if thread_ts.is_some() { list.iter().rev().find(mine) } else { list.iter().find(mine) };
                if let Some(m) = last.and_then(|m| m["id"].as_str()) {
                    chat.editing.set(Some(m.to_owned()));
                }
            });
        }
    };
    let placeholder = if in_thread { "Reply…".to_owned() } else { format!("Message {}", place.name) };
    let channel_name = place.name.clone();
    let place_name = StoredValue::new(place.name.clone());
    let tool =
        "rounded px-1.5 py-0.5 text-xs text-zinc-500 hover:bg-zinc-200 hover:text-zinc-900 dark:hover:bg-zinc-800 dark:hover:text-zinc-100";
    let s2 = send.clone();
    view! {
        <div class="relative px-4 pb-3 pt-1">
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || pending.get().map(|(t, _)| view! {
                <div class="mb-1 flex items-center gap-2 rounded-md bg-amber-50 px-2 py-1 text-xs text-amber-900 dark:bg-amber-950 dark:text-amber-200">
                    <span class="min-w-0 flex-1 truncate">{format!("Sending in a few seconds: {t}")}</span>
                    <button class="underline" on:click=move |_| { pending.set(None); text.set(t.clone()); }>"Undo"</button>
                </div>
            })}
            {move || options.get().map(|list| view! {
                <div class="absolute bottom-full left-4 right-4 z-30 mb-1 overflow-hidden rounded-md border border-zinc-200 bg-white text-sm shadow-lg dark:border-zinc-700 dark:bg-zinc-900">
                    {list.into_iter().enumerate().map(|(i, (insert, label, note))| view! {
                        <button class=move || format!("flex w-full items-center gap-2 px-3 py-1 text-left {}", if picked.get() == i { "bg-sky-700 text-white" } else { "hover:bg-zinc-100 dark:hover:bg-zinc-800" })
                            on:mousedown=move |ev| { ev.prevent_default(); complete(insert.clone()); }>
                            <span>{label}</span><span class="truncate text-xs opacity-60">{note}</span>
                        </button>
                    }).collect_view()}
                </div>
            })}
            <div class="rounded-lg border border-zinc-300 focus-within:border-zinc-500 dark:border-zinc-700 dark:focus-within:border-zinc-500">
                <div class="flex flex-wrap gap-0.5 border-b border-zinc-200 px-1 py-0.5 dark:border-zinc-800">
                    <button class=format!("{tool} font-bold") title="Bold" on:click=move |_| wrap("*", "*")>"B"</button>
                    <button class=format!("{tool} italic") title="Italic" on:click=move |_| wrap("_", "_")>"I"</button>
                    <button class=format!("{tool} line-through") title="Strikethrough" on:click=move |_| wrap("~", "~")>"S"</button>
                    <button class=format!("{tool} font-mono") title="Code" on:click=move |_| wrap("`", "`")>"</>"</button>
                    <button class=tool title="Link" on:click=move |_| wrap("", " (https://)")>"🔗"</button>
                    <button class=tool title="Bulleted list" on:click=move |_| line("• ")>"•≡"</button>
                    <button class=tool title="Numbered list" on:click=move |_| line("1. ")>"1≡"</button>
                    <button class=tool title="Quote" on:click=move |_| line("> ")>"❝"</button>
                    <button class=format!("{tool} font-mono") title="Code block" on:click=move |_| wrap("```\n", "\n```")>"{ }"</button>
                </div>
                <textarea node_ref=area class="block max-h-60 min-h-[2.5rem] w-full resize-none bg-transparent px-3 py-2 text-sm outline-none" rows="2"
                    placeholder=placeholder prop:value=move || text.get()
                    on:input=move |ev| {
                        text.set(event_target_value(&ev));
                        if let Some(el) = area.get_untracked() { caret.set(el.selection_start().ok().flatten().unwrap_or(0) as usize); }
                    }
                    on:click=move |_| if let Some(el) = area.get_untracked() { caret.set(el.selection_start().ok().flatten().unwrap_or(0) as usize); }
                    on:keydown={
                        let send = send.clone();
                        let edit_last = edit_last.clone();
                        move |ev: web_sys::KeyboardEvent| {
                            if let Some(list) = options.get_untracked() {
                                match ev.key().as_str() {
                                    "ArrowDown" => { ev.prevent_default(); picked.update(|p| *p = (*p + 1).min(list.len() - 1)); return; }
                                    "ArrowUp" => { ev.prevent_default(); picked.update(|p| *p = p.saturating_sub(1)); return; }
                                    "Enter" | "Tab" => {
                                        ev.prevent_default();
                                        if let Some((insert, _, _)) = list.get(picked.get_untracked()) { complete(insert.clone()); }
                                        return;
                                    }
                                    "Escape" => { caret.set(0); return; }
                                    _ => {}
                                }
                            }
                            let cmd = ev.meta_key() || ev.ctrl_key();
                            match ev.key().as_str() {
                                "Enter" if !ev.shift_key() => { ev.prevent_default(); send(); }
                                "ArrowUp" if text.get_untracked().is_empty() => { ev.prevent_default(); edit_last(); }
                                "b" if cmd => { ev.prevent_default(); wrap("*", "*"); }
                                "i" if cmd => { ev.prevent_default(); wrap("_", "_"); }
                                "x" if cmd && ev.shift_key() => { ev.prevent_default(); wrap("~", "~"); }
                                _ => {}
                            }
                        }
                    }
                    on:keyup=move |ev: web_sys::KeyboardEvent| {
                        if matches!(ev.key().as_str(), "ArrowLeft" | "ArrowRight" | "Home" | "End") && let Some(el) = area.get_untracked() {
                            caret.set(el.selection_start().ok().flatten().unwrap_or(0) as usize);
                        }
                    }></textarea>
                <div class="flex items-center gap-2 px-2 pb-1 text-[11px] text-zinc-500">
                    {in_thread.then(|| view! {
                        <label class="flex items-center gap-1">
                            <input type="checkbox" prop:checked=move || broadcast.get() on:change=move |ev| broadcast.set(event_target_checked(&ev)) />
                            {format!("Also send to {channel_name}")}
                        </label>
                    })}
                    <crate::compose::WorkInSession key=target.get_value() about=place_name.get_value() current=text set=Callback::new(move |t: String| text.set(t)) />
                    <span class="flex-1"></span>
                    {(!in_thread).then(|| view! { <span>"Enter to send · Shift+Enter for a new line · ↑ to edit your last"</span> })}
                    <button class="rounded bg-sky-700 px-2 py-0.5 text-white disabled:opacity-40" disabled=move || text.get().trim().is_empty() on:click=move |_| s2()>"Send"</button>
                </div>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_is_being_completed() {
        assert_eq!(completing("hi @an", 6), Some(('@', "an".into(), 3)));
        assert_eq!(completing("see #en", 7), Some(('#', "en".into(), 4)));
        assert_eq!(completing("yay :ta", 7), Some((':', "ta".into(), 4)));
        assert_eq!(completing("yay :t", 6), None, "two letters for emoji");
        assert_eq!(completing("a@b", 3), None);
        assert_eq!(completing("done :tada: ", 12), None);
    }
}
