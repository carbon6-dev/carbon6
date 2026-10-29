//! The model field everywhere a model is chosen: type any id, or pick from
//! the connection's models. The list always shows every model, whatever is
//! typed (a datalist would hide those not matching the text).

use leptos::prelude::*;

use crate::api;

/// The models offered: by connection, each its list or why not.
type Offered = Vec<(String, Result<Vec<String>, String>)>;

/// A list of models, or why there is none; None until asked for.
type Listed = Signal<Option<Result<Vec<String>, String>>>;

/// `connection`: an id or a name; empty, every connection's models, by
/// connection. `list`: the models from elsewhere (a connection being set up
/// and tested), instead of asking the head.
#[component]
pub fn ModelInput(
    #[prop(into)] connection: Signal<String>,
    #[prop(into)] value: Signal<String>,
    #[prop(into)] on_set: Callback<String>,
    #[prop(optional, into)] placeholder: Signal<String>,
    #[prop(optional, into)] class: String,
    #[prop(optional, into)] disabled: Signal<bool>,
    #[prop(optional)] list: Option<Listed>,
) -> impl IntoView {
    let shown = RwSignal::new(false);
    let connections = LocalResource::new(api::connections);
    // Asked for when the list is first shown, and again for another connection.
    let offered = LocalResource::new(move || {
        let (c, open) = (connection.get(), shown.get() && !disabled.get());
        let all = connections.get().and_then(Result::ok);
        async move {
            let all = all.filter(|_| open && list.is_none())?;
            let chosen: Vec<_> = all.into_iter().filter(|x| c.is_empty() || x.id == c || x.name == c).collect();
            let mut out = Offered::new();
            for x in chosen {
                out.push((x.name.clone(), api::models(x.id.clone()).await));
            }
            Some(out)
        }
    });
    let groups = move || -> Option<Offered> {
        match list {
            Some(l) => l.get().map(|r| vec![(String::new(), r)]),
            None => offered.get().flatten(),
        }
    };
    let pick = move |m: String| {
        on_set.run(m);
        shown.set(false);
    };
    // Opened where there's room: up from a box near the bottom of the
    // window (a session's composer), and no taller than the room there.
    let wrap = NodeRef::<leptos::html::Div>::new();
    let room = RwSignal::new((false, 288.0));
    Effect::new(move || {
        if shown.get()
            && let Some(el) = wrap.get_untracked()
        {
            let r = el.get_bounding_client_rect();
            let height = window().inner_height().ok().and_then(|h| h.as_f64()).unwrap_or(800.0);
            let (below, above) = (height - r.bottom() - 12.0, r.top() - 12.0);
            let up = below < 288.0 && above > below;
            room.set((up, if up { above } else { below }.clamp(96.0, 288.0)));
        }
    });
    view! {
        <div class="relative" node_ref=wrap>
            <div class="flex">
                <input class=format!("{class} rounded-r-none font-mono") autocomplete="off" placeholder=move || placeholder.get()
                    prop:disabled=move || disabled.get()
                    prop:value=move || value.get() on:input=move |ev| on_set.run(event_target_value(&ev))
                    on:focus=move |_| shown.set(true) on:blur=move |_| shown.set(false)
                    on:keydown=move |ev| if ev.key() == "Escape" { shown.set(false) } />
                <button type="button" tabindex="-1" title="Every model the connection offers"
                    prop:disabled=move || disabled.get()
                    class="rounded-r-md border border-l-0 border-zinc-300 px-2 text-xs text-zinc-500 hover:bg-zinc-100 dark:border-zinc-700 dark:hover:bg-zinc-800"
                    on:mousedown=move |ev| { ev.prevent_default(); shown.update(|s| *s = !*s); }>"▾"</button>
            </div>
            {move || (shown.get() && !disabled.get()).then(|| view! {
                <div class=move || format!("absolute z-30 w-full min-w-56 overflow-auto rounded-md border border-zinc-200 bg-white py-1 text-xs shadow-lg dark:border-zinc-700 dark:bg-zinc-900 {}",
                        if room.get().0 { "bottom-full mb-1" } else { "mt-1" })
                    style=move || format!("max-height: {}px", room.get().1)
                    on:mousedown=|ev| ev.prevent_default()>
                    {move || match groups() {
                        None => view! { <p class="px-2 py-1 text-zinc-500">{if list.is_some() { "Test the connection to list its models." } else { "Asking for the models…" }}</p> }.into_any(),
                        Some(gs) => {
                            let several = gs.len() > 1;
                            if gs.is_empty() {
                                return view! { <p class="px-2 py-1 text-zinc-500">"No connections."</p> }.into_any();
                            }
                            gs.into_iter().map(|(name, r)| view! {
                                {several.then(|| view! { <p class="px-2 pt-1.5 pb-0.5 text-[10px] font-semibold uppercase tracking-wide text-zinc-500">{name.clone()}</p> })}
                                {match r {
                                    Err(e) => view! { <p class="px-2 py-1 text-red-600">{e}</p> }.into_any(),
                                    Ok(ms) if ms.is_empty() => view! { <p class="px-2 py-1 text-zinc-500">"It lists no models; type one."</p> }.into_any(),
                                    Ok(ms) => ms.into_iter().map(|m| {
                                        let (chosen, label) = (m.clone(), m.clone());
                                        let current = move || value.get() == label;
                                        view! {
                                            <button type="button" class=move || format!("block w-full truncate px-2 py-1 text-left font-mono hover:bg-zinc-100 dark:hover:bg-zinc-800 {}", if current() { "bg-sky-50 text-sky-800 dark:bg-sky-950 dark:text-sky-300" } else { "" })
                                                on:click=move |_| pick(chosen.clone())>{m}</button>
                                        }
                                    }).collect_view().into_any(),
                                }}
                            }).collect_view().into_any()
                        }
                    }}
                </div>
            })}
        </div>
    }
}
