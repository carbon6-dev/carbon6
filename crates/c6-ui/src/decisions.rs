//! Decisions (docs/os.md §4.2): every time carbon6 needs you, in one list —
//! agents' questions, approvals of tool calls and workflow steps, the
//! assistant's questions, facts to confirm, disputes. Answer one here
//! (an option, a form, a note), snooze or dismiss it, or start a session
//! that helps you settle it.

use c6_api::Decision;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT, ago, now_ms};
use crate::workspace::{Go, How, View, use_workspace};

/// The header link, with how many are open.
#[component]
pub fn DecisionLink(class: &'static str) -> impl IntoView {
    let live = api::live();
    let tick = RwSignal::new(0u32);
    let handle = set_interval_with_handle(move || tick.update(|t| *t += 1), std::time::Duration::from_secs(30)).ok();
    on_cleanup(move || {
        if let Some(h) = handle {
            h.clear();
        }
    });
    let count = LocalResource::new(move || {
        tick.track();
        live.sessions.track();
        api::decision_count()
    });
    view! {
        <Go to=View::Decisions class=class>
            "Decisions"
            {move || count.get().and_then(Result::ok).filter(|n| *n > 0).map(|n| view! {
                <span class="ml-1 rounded-full bg-sky-600 px-1.5 text-[10px] font-semibold text-white">{n}</span>
            })}
        </Go>
    }
}

#[component]
pub fn DecisionsPage() -> impl IntoView {
    let live = api::live();
    let state = RwSignal::new("open".to_owned());
    let tick = RwSignal::new(0u32);
    let list = LocalResource::new(move || {
        tick.track();
        live.sessions.track();
        let s = state.get();
        let realm = api::realm().get();
        async move { api::decisions(s).await.map(|ds| ds.into_iter().filter(|d| realm.is_empty() || d.realm == realm).collect::<Vec<_>>()) }
    });
    let refresh = Callback::new(move |()| tick.update(|t| *t += 1));
    view! {
        <main class="mx-auto max-w-3xl space-y-4 px-4 py-6">
            <div class="flex flex-wrap items-center gap-2">
                <h1 class="text-lg font-semibold">"Decisions"</h1>
                <span class="flex-1 text-sm text-zinc-500">"Everything that needs you. What something waits on comes first; agents that asked without stopping carry on, and get your answer as a message."</span>
                <select class=format!("{INPUT} w-auto py-1 text-sm") prop:value=move || state.get() on:change=move |ev| state.set(event_target_value(&ev))>
                    <option value="open">"Open"</option>
                    <option value="snoozed">"Snoozed"</option>
                    <option value="answered">"Answered"</option>
                    <option value="expired">"Expired"</option>
                    <option value="dismissed">"Dismissed"</option>
                    <option value="withdrawn">"Withdrawn"</option>
                </select>
            </div>
            <Suspense fallback=|| ()>
                {move || list.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                    Ok(v) if v.is_empty() => view! { <p class="text-sm text-zinc-500">"Nothing here."</p> }.into_any(),
                    Ok(v) => {
                        // Grouped: each group's decisions together, under its name.
                        let mut groups: Vec<(String, Vec<Decision>)> = Vec::new();
                        for d in v {
                            match groups.iter_mut().find(|(g, _)| *g == d.group) {
                                Some((_, ds)) => ds.push(d),
                                None => groups.push((d.group.clone(), vec![d])),
                            }
                        }
                        groups.into_iter().map(|(g, ds)| view! {
                            <div class="space-y-2">
                                {(!g.is_empty()).then(|| view! { <h2 class="text-xs font-medium uppercase tracking-wide text-zinc-500">{format!("{g} ({})", ds.len())}</h2> })}
                                {ds.into_iter().map(|d| view! { <DecisionCard d refresh /> }).collect_view()}
                            </div>
                        }).collect_view().into_any()
                    }
                })}
            </Suspense>
        </main>
    }
}

#[component]
fn DecisionCard(d: Decision, refresh: Callback<()>) -> impl IntoView {
    let ws = use_workspace();
    let id = d.id;
    let note = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    let values: Vec<(c6_api::FormField, RwSignal<String>)> = d.form.iter().map(|f| (f.clone(), RwSignal::new(String::new()))).collect();
    let values = StoredValue::new(values);
    let set = move |body: serde_json::Value| {
        spawn_local(async move {
            match api::edit_decision(id, body).await {
                Ok(_) => refresh.run(()),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let answer = move |option: Option<String>| {
        let mut fields = serde_json::Map::new();
        for (f, v) in values.get_value() {
            let text = v.get_untracked();
            if !text.trim().is_empty() {
                fields.insert(f.name.clone(), json!(text));
            }
        }
        let a = c6_api::DecisionAnswer { option, values: fields, note: note.get_untracked() };
        busy.set(true);
        spawn_local(async move {
            match api::answer_decision(id, a).await {
                Ok(_) => refresh.run(()),
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    let start = move |_| {
        busy.set(true);
        spawn_local(async move {
            match api::decision_session(id).await {
                Ok(s) => {
                    ws.open(None, View::session(s.id), How::Beside);
                    refresh.run(());
                }
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        });
    };
    let facts: Vec<String> =
        d.context["facts"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default();
    let things: Vec<String> =
        d.context["things"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default();
    let session = d.context["session"].as_str().map(str::to_owned);
    let open = matches!(d.state.as_str(), "open" | "snoozed");
    let tone = match (d.waiting && open, d.priority.as_str()) {
        (true, _) => "border-sky-300 dark:border-sky-900",
        (_, "high") => "border-amber-300 dark:border-amber-900",
        _ => "border-zinc-200 dark:border-zinc-800",
    };
    let has_form = !d.form.is_empty();
    let deadline = d.deadline.filter(|_| open).map(|t| {
        let left = t - now_ms();
        let default = d.default_answer.clone().map(|a| format!(" (then: {a})")).unwrap_or_else(|| " (then: no)".into());
        if left > 0 { format!("answer within {}{default}", ago(left).trim_end_matches(" ago")) } else { "due now".to_owned() }
    });
    let answered = d.answer.as_ref().map(|a| {
        let mut parts: Vec<String> = a["option"].as_str().map(str::to_owned).into_iter().collect();
        if let Some(vs) = a["values"].as_object() {
            parts.extend(vs.iter().map(|(k, v)| format!("{k}: {}", v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()))));
        }
        format!("{} {}", parts.join(" · "), if d.answered_via.is_empty() { String::new() } else { format!("(via {})", d.answered_via) })
    });
    view! {
        <section class=format!("space-y-2 rounded-lg border p-3 {tone}")>
            <div class="flex flex-wrap items-center gap-2">
                <span class="rounded bg-zinc-100 px-1.5 text-[11px] dark:bg-zinc-800">{d.kind.clone()}</span>
                {(d.waiting && open).then(|| view! { <span class="rounded bg-sky-100 px-1.5 text-[11px] text-sky-800 dark:bg-sky-950 dark:text-sky-300">"waiting"</span> })}
                <h2 class="min-w-0 flex-1 font-medium">{d.title.clone()}</h2>
                {(d.count > 1).then(|| view! { <span class="text-xs text-zinc-500">{format!("raised {}×", d.count)}</span> })}
                <span class="text-xs text-zinc-500">{format!("{} ago", ago(now_ms() - d.updated_at))}</span>
            </div>
            {(!d.document.is_empty()).then(|| view! { <div class="md text-sm text-zinc-700 dark:text-zinc-300" inner_html=crate::markdown::to_html(&d.document)></div> })}
            <div class="flex flex-wrap items-center gap-2 text-xs text-zinc-500">
                <span>{format!("from {}", d.source)}</span>
                {session.map(|s| view! { <Go to=View::session(s) class="text-sky-700 hover:underline dark:text-sky-400">"the session ↗"</Go> })}
                {things.into_iter().map(|k| view! { <span class="rounded bg-violet-100 px-1 font-mono text-violet-800 dark:bg-violet-950 dark:text-violet-300">{k}</span> }).collect_view()}
                {d.sessions.iter().map(|s| view! { <Go to=View::session(s.clone()) class="text-sky-700 hover:underline dark:text-sky-400">"working session ↗"</Go> }).collect_view()}
                {deadline.map(|t| view! { <span class="text-amber-700 dark:text-amber-400">{t}</span> })}
            </div>
            {(!facts.is_empty()).then(|| view! { <FactList ids=facts on_change=refresh /> })}
            {answered.map(|a| view! { <p class="text-sm text-emerald-700 dark:text-emerald-400">{format!("Answered: {a}")}</p> })}
            {(!d.note.is_empty()).then(|| view! { <p class="text-sm text-zinc-600 dark:text-zinc-400">{format!("Note: {}", d.note)}</p> })}
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {open.then(|| view! {
                {values.get_value().into_iter().map(|(f, v)| {
                    let label = format!("{}{}", if f.label.is_empty() { f.name.clone() } else { f.label.clone() }, if f.required { " *" } else { "" });
                    let input = if !f.choices.is_empty() {
                        view! {
                            <select class=INPUT prop:value=move || v.get() on:change=move |ev| v.set(event_target_value(&ev))>
                                <option value="">"—"</option>
                                {f.choices.iter().map(|c| view! { <option value=c.clone()>{c.clone()}</option> }).collect_view()}
                            </select>
                        }.into_any()
                    } else if f.kind == "long" {
                        view! { <textarea class=format!("{INPUT} h-20 text-sm") prop:value=move || v.get() on:input=move |ev| v.set(event_target_value(&ev))></textarea> }.into_any()
                    } else {
                        view! { <input class=INPUT prop:value=move || v.get() on:input=move |ev| v.set(event_target_value(&ev)) /> }.into_any()
                    };
                    view! { <label class="block space-y-1"><span class="text-xs text-zinc-500">{label}</span>{input}</label> }
                }).collect_view()}
                <input class=format!("{INPUT} py-1 text-sm") placeholder="A note with your answer (optional)"
                    prop:value=move || note.get() on:input=move |ev| note.set(event_target_value(&ev)) />
                <div class="flex flex-wrap items-center gap-2">
                    {d.options.iter().map(|o| {
                        let label = o.label.clone();
                        view! {
                            <button class=BUTTON title=o.does.clone() disabled=move || busy.get() on:click=move |_| answer(Some(label.clone()))>{o.label.clone()}</button>
                        }
                    }).collect_view()}
                    {(d.options.is_empty()).then(|| view! {
                        <button class=BUTTON disabled=move || busy.get() on:click=move |_| answer(None)>{if has_form { "Send" } else { "Settled" }}</button>
                    })}
                    {(!d.waiting).then(|| view! {
                        <button class=BUTTON_QUIET disabled=move || busy.get() on:click=start title="A session that helps you settle it, with its context">"Work on it"</button>
                        <button class=BUTTON_QUIET on:click=move |_| set(json!({"state": "snoozed", "snooze_until": now_ms() + 86_400_000}))>"Tomorrow"</button>
                        <button class=format!("{BUTTON_QUIET} text-zinc-500") on:click=move |_| set(json!({"state": "dismissed"}))>"Dismiss"</button>
                    })}
                </div>
            })}
            {(!open && d.state == "dismissed").then(|| view! { <button class=BUTTON_QUIET on:click=move |_| set(json!({"state": "open"}))>"Reopen"</button> })}
        </section>
    }
}

/// Facts (by id) with what to do about each: confirm, retire, correct.
#[component]
pub fn FactList(ids: Vec<String>, #[prop(optional)] on_change: Option<Callback<()>>) -> impl IntoView {
    let ids = StoredValue::new(ids);
    let tick = RwSignal::new(0u32);
    let facts = LocalResource::new(move || {
        tick.track();
        api::rpc_as::<Vec<c6_api::Fact>>("memex.knowledge.get", json!({"ids": ids.get_value()}))
    });
    let changed = move || {
        tick.update(|t| *t += 1);
        if let Some(c) = on_change {
            c.run(());
        }
    };
    view! {
        <Suspense fallback=|| ()>
            {move || facts.get().map(|r| match r {
                Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                Ok(fs) => view! {
                    <ul class="space-y-2">{fs.into_iter().map(|f| view! { <FactRow f on_change=Callback::new(move |()| changed()) /> }).collect_view()}</ul>
                }.into_any(),
            })}
        </Suspense>
    }
}

pub fn status_tone(status: &str) -> &'static str {
    match status {
        "reviewed" => "bg-emerald-100 text-emerald-800 dark:bg-emerald-950 dark:text-emerald-300",
        "verified" => "bg-sky-100 text-sky-800 dark:bg-sky-950 dark:text-sky-300",
        "observed" => "bg-zinc-100 text-zinc-700 dark:bg-zinc-800 dark:text-zinc-300",
        "inferred" => "bg-violet-100 text-violet-800 dark:bg-violet-950 dark:text-violet-300",
        "disputed" => "bg-amber-100 text-amber-900 dark:bg-amber-950 dark:text-amber-200",
        _ => "bg-zinc-100 text-zinc-400 line-through dark:bg-zinc-900",
    }
}

#[component]
pub fn FactRow(f: c6_api::Fact, on_change: Callback<()>) -> impl IntoView {
    let id = StoredValue::new(f.id.clone());
    let fixing = RwSignal::new(false);
    let text = RwSignal::new(f.claim.clone());
    let error = RwSignal::new(None::<String>);
    let send = move |body: serde_json::Value| {
        spawn_local(async move {
            let mut b = body;
            b["id"] = json!(id.get_value());
            match api::rpc("memex.knowledge.set", b).await {
                Ok(_) => on_change.run(()),
                Err(e) => error.set(Some(e)),
            }
        });
    };
    let current = f.valid_to.is_none();
    let last = f.evidence.last().cloned().unwrap_or_default();
    view! {
        <li class="space-y-1 rounded-md bg-zinc-50 p-2 text-sm dark:bg-zinc-900">
            <div class="flex flex-wrap items-center gap-2">
                <span class=format!("rounded px-1.5 text-[11px] {}", status_tone(&f.status))>{f.status.clone()}</span>
                <span class="text-[11px] text-zinc-500">{f.volatility.clone()}{f.due.then_some(" · due")}</span>
                {(!f.labels.is_empty()).then(|| view! { <span class="text-[11px] text-zinc-500">{format!("only {}", f.labels.join(", "))}</span> })}
            </div>
            <p>{f.negative.then_some("NOT: ")}{f.claim.clone()}</p>
            {(!f.conditions.is_empty()).then(|| view! { <p class="text-xs text-zinc-500">{format!("when: {}", f.conditions)}</p> })}
            {f.check.clone().map(|c| view! { <p class="font-mono text-[11px] text-zinc-500">{format!("check: {}", c.command)}</p> })}
            {(!last.is_null()).then(|| {
                let what = [last["command"].as_str(), last["source"].as_str()].into_iter().flatten().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
                view! {
                    <details class="text-xs text-zinc-500">
                        <summary class="cursor-pointer">{format!("evidence ({}) {}", f.evidence.len(), last["by"].as_str().unwrap_or(""))}</summary>
                        <p class="font-mono">{what}</p>
                        <pre class="max-h-40 overflow-auto whitespace-pre-wrap">{last["excerpt"].as_str().unwrap_or("").to_owned()}</pre>
                    </details>
                }
            })}
            {move || error.get().map(|e| view! { <p class="text-xs text-red-600">{e}</p> })}
            {current.then(|| view! {
                <div class="flex flex-wrap gap-2 text-xs">
                    {(f.status != "reviewed").then(|| view! { <button class="text-emerald-700 hover:underline dark:text-emerald-400" on:click=move |_| send(json!({"status": "reviewed"}))>"Confirm"</button> })}
                    <button class="text-sky-700 hover:underline dark:text-sky-400" on:click=move |_| fixing.update(|x| *x = !*x)>"Correct"</button>
                    <button class="text-red-600 hover:underline" on:click=move |_| send(json!({"status": "retired"}))>"No longer true"</button>
                </div>
            })}
            {move || fixing.get().then(|| view! {
                <div class="flex gap-2">
                    <input class=format!("{INPUT} min-w-0 flex-1 py-1 text-sm") prop:value=move || text.get() on:input=move |ev| text.set(event_target_value(&ev)) />
                    <button class=BUTTON on:click=move |_| send(json!({"claim": text.get_untracked()}))>"Save"</button>
                </div>
            })}
        </li>
    }
}
