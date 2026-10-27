//! Contacts: your Google contacts (synced both ways), and people you write
//! to who are not contacts yet, to add.

use c6_api::{Contact, ContactCandidate};
use std::collections::HashSet;

use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;

use crate::api;
use crate::util::{BUTTON_QUIET, ErrorBox, INPUT};
use crate::workspace::{Go, View};

#[component]
pub fn ContactsPage() -> impl IntoView {
    let q = RwSignal::new(String::new());
    let refresh = RwSignal::new(0u32);
    let list = LocalResource::new(move || {
        let (q, _) = (q.get(), refresh.get());
        api::rpc_as::<Vec<Contact>>("memex.contacts.list", json!({"q": q}))
    });
    let candidates = LocalResource::new(move || {
        let _ = refresh.get();
        api::rpc_as::<Vec<ContactCandidate>>("memex.contacts.candidates", json!({}))
    });
    let accounts = LocalResource::new(api::google_accounts);
    let error = RwSignal::new(None::<String>);
    // Who is being added (their buttons wait), who was, and who is ticked.
    let adding = RwSignal::new(HashSet::<String>::new());
    let added = RwSignal::new(HashSet::<String>::new());
    let ticked = RwSignal::new(HashSet::<String>::new());
    let all = RwSignal::new(false);
    // Clicks a moment apart go together, in one run of the contacts app.
    let queue = RwSignal::new(Vec::<(String, String)>::new());
    let send = move |people: Vec<(String, String)>| {
        let people: Vec<(String, String)> =
            people.into_iter().filter(|(_, e)| !adding.get_untracked().contains(e) && !added.get_untracked().contains(e)).collect();
        if people.is_empty() {
            return;
        }
        adding.update(|a| a.extend(people.iter().map(|(_, e)| e.clone())));
        ticked.update(|t| {
            people.iter().for_each(|(_, e)| {
                t.remove(e);
            })
        });
        let account = accounts.get_untracked().and_then(Result::ok).and_then(|a| a.first().map(|x| x.email.clone())).unwrap_or_default();
        spawn_local(async move {
            let list: Vec<_> = people.iter().map(|(n, e)| json!({"name": n, "email": e})).collect();
            match api::rpc("memex.contacts.create_many", json!({"account": account, "people": list})).await {
                Ok(_) => {
                    added.update(|a| a.extend(people.iter().map(|(_, e)| e.clone())));
                    refresh.update(|n| *n += 1);
                }
                Err(e) => error.set(Some(e)),
            }
            adding.update(|a| {
                people.iter().for_each(|(_, e)| {
                    a.remove(e);
                })
            });
        });
    };
    let click = move |name: String, email: String| {
        let first = queue.with_untracked(Vec::is_empty);
        adding.update(|a| {
            a.insert(email.clone());
        });
        queue.update(|q| q.push((name, email)));
        if first {
            set_timeout(
                move || {
                    let people = queue.try_update(std::mem::take).unwrap_or_default();
                    adding.update(|a| {
                        people.iter().for_each(|(_, e)| {
                            a.remove(e);
                        })
                    });
                    send(people);
                },
                std::time::Duration::from_millis(800),
            );
        }
    };
    view! {
        <main class="mx-auto max-w-4xl space-y-4 px-4 py-4">
            <input class=INPUT placeholder="Search contacts" prop:value=move || q.get() on:input=move |ev| q.set(event_target_value(&ev)) />
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || candidates.get().and_then(Result::ok).map(|cs| cs.into_iter().filter(|c| !added.get().contains(&c.email)).collect::<Vec<_>>()).filter(|c| !c.is_empty()).map(|cs| {
                let total = cs.len();
                let everyone: Vec<(String, String)> = cs.iter().map(|c| (c.name.clone(), c.email.clone())).collect();
                let shown = if all.get() { total } else { total.min(15) };
                let for_ticked = everyone.clone();
                view! {
                <section class="space-y-1 rounded-lg border border-amber-200 bg-amber-50/50 p-3 dark:border-amber-900 dark:bg-amber-950/30">
                    <div class="flex flex-wrap items-center gap-2">
                        <h2 class="flex-1 text-sm font-medium">{format!("People you write to who are not contacts ({total})")}</h2>
                        <button class=format!("{BUTTON_QUIET} text-xs") disabled=move || ticked.with(HashSet::is_empty)
                            on:click=move |_| { let t = ticked.get_untracked(); send(for_ticked.iter().filter(|(_, e)| t.contains(e)).cloned().collect()); }>
                            {move || format!("Add selected ({})", ticked.with(HashSet::len))}
                        </button>
                        <button class=format!("{BUTTON_QUIET} text-xs") on:click=move |_| send(everyone.clone())>{format!("Add all {total}")}</button>
                    </div>
                    {cs.into_iter().take(shown).map(|c| {
                        let (name, email) = (c.name.clone(), c.email.clone());
                        let (e1, e2, e3) = (email.clone(), email.clone(), email.clone());
                        view! {
                            <label class="flex flex-wrap items-center gap-2 text-sm">
                                <input type="checkbox" prop:checked=move || ticked.with(|t| t.contains(&e1))
                                    on:change=move |ev| { let on = event_target_checked(&ev); ticked.update(|t| { if on { t.insert(e2.clone()); } else { t.remove(&e2); } }); } />
                                <span>{if c.name.is_empty() { c.email.clone() } else { format!("{} <{}>", c.name, c.email) }}</span>
                                <span class="text-xs text-zinc-500">{format!("you wrote {} · they wrote {}", c.sent, c.received)}</span>
                                <span class="flex-1"></span>
                                <button class=BUTTON_QUIET disabled=move || adding.with(|a| a.contains(&e3))
                                    on:click=move |ev| { ev.prevent_default(); click(name.clone(), email.clone()); }>
                                    {let e = c.email.clone(); move || if adding.with(|a| a.contains(&e)) { "Adding…" } else { "Add to Google contacts" }}
                                </button>
                            </label>
                        }
                    }).collect_view()}
                    {(total > shown).then(|| view! {
                        <button class="text-xs text-sky-700 hover:underline dark:text-sky-400" on:click=move |_| all.set(true)>{format!("Show all {total}")}</button>
                    })}
                </section>
            }})}
            <Suspense fallback=|| ()>
                {move || list.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=format!("{e} (Connect a Google account in Settings → Mail & calendar.)") /> }.into_any(),
                    Ok(v) if v.is_empty() => view! { <p class="text-sm text-zinc-500">"No contacts."</p> }.into_any(),
                    Ok(v) => view! {
                        <div class="divide-y divide-zinc-200 rounded-lg border border-zinc-200 text-sm dark:divide-zinc-800 dark:border-zinc-800">
                            {v.into_iter().map(|c| view! {
                                <div class="flex flex-wrap items-baseline gap-x-3 px-3 py-2">
                                    <span class="font-medium">{c.name.clone()}</span>
                                    <span class="text-xs text-zinc-500">{c.emails.join(", ")}</span>
                                    <span class="text-xs text-zinc-500">{c.orgs.join(", ")}</span>
                                    <span class="text-xs text-zinc-500">{c.phones.join(", ")}</span>
                                    <span class="flex-1"></span>
                                    {c.emails.first().map(|e| view! { <Go to=View::Memex class="text-xs text-sky-700 hover:underline dark:text-sky-400">{format!("in the memex: person:{e}")}</Go> })}
                                </div>
                            }).collect_view()}
                        </div>
                    }.into_any(),
                })}
            </Suspense>
        </main>
    }
}
