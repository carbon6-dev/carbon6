//! Contacts: your Google contacts (synced both ways), and people you write
//! to who are not contacts yet, to add.

use c6_api::{Contact, ContactCandidate};
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
    view! {
        <main class="mx-auto max-w-4xl space-y-4 px-4 py-4">
            <input class=INPUT placeholder="Search contacts" prop:value=move || q.get() on:input=move |ev| q.set(event_target_value(&ev)) />
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || candidates.get().and_then(Result::ok).filter(|c| !c.is_empty()).map(|cs| view! {
                <section class="space-y-1 rounded-lg border border-amber-200 bg-amber-50/50 p-3 dark:border-amber-900 dark:bg-amber-950/30">
                    <h2 class="text-sm font-medium">"People you write to who are not contacts"</h2>
                    {cs.into_iter().take(15).map(|c| {
                        let (name, email) = (c.name.clone(), c.email.clone());
                        view! {
                            <div class="flex flex-wrap items-center gap-2 text-sm">
                                <span>{if c.name.is_empty() { c.email.clone() } else { format!("{} <{}>", c.name, c.email) }}</span>
                                <span class="text-xs text-zinc-500">{format!("you wrote {} · they wrote {}", c.sent, c.received)}</span>
                                <span class="flex-1"></span>
                                <button class=BUTTON_QUIET on:click=move |_| {
                                    let (name, email) = (name.clone(), email.clone());
                                    let account = accounts.get_untracked().and_then(Result::ok).and_then(|a| a.first().map(|x| x.email.clone())).unwrap_or_default();
                                    spawn_local(async move {
                                        match api::rpc("memex.contacts.create", json!({"account": account, "name": name, "email": email})).await {
                                            Ok(_) => refresh.update(|n| *n += 1),
                                            Err(e) => error.set(Some(e)),
                                        }
                                    });
                                }>"Add to Google contacts"</button>
                            </div>
                        }
                    }).collect_view()}
                </section>
            })}
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
