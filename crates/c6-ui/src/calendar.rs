//! Calendar: every calendar of your Google accounts, through the memex.
//! Agenda, week and month; an event's details with RSVP, edit and delete
//! (this one, following, all); new events.

use c6_api::{Calendar, CalendarEvent};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::json;

use crate::api;
use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT};

pub(crate) const DAY: f64 = 86_400_000.0;

pub(crate) fn date_of(ms: f64) -> js_sys::Date {
    js_sys::Date::new(&wasm_bindgen::JsValue::from_f64(ms))
}

/// Local midnight of the day containing `ms`.
pub(crate) fn midnight(ms: f64) -> f64 {
    let d = date_of(ms);
    d.set_hours(0);
    d.set_minutes(0);
    d.set_seconds(0);
    d.set_milliseconds(0);
    d.get_time()
}

pub(crate) fn hm(ms: i64) -> String {
    let d = date_of(ms as f64);
    format!("{:02}:{:02}", d.get_hours(), d.get_minutes())
}

pub(crate) fn day_label(ms: f64) -> String {
    String::from(date_of(ms).to_locale_date_string(
        "default",
        &js_sys::Object::from(js_sys::JSON::parse(r#"{"weekday":"short","month":"short","day":"numeric"}"#).unwrap()),
    ))
}

#[component]
pub fn CalendarPage() -> impl IntoView {
    let mode = RwSignal::new("week");
    // The first day shown.
    let anchor = RwSignal::new(midnight(js_sys::Date::now()));
    let range = move || -> (f64, f64) {
        let a = anchor.get();
        match mode.get() {
            "month" => {
                let d = date_of(a);
                d.set_date(1);
                let first = midnight(d.get_time());
                // From the Monday on or before the 1st, six weeks.
                let back = ((date_of(first).get_day() + 6) % 7) as f64;
                let start = first - back * DAY;
                (start, start + 42.0 * DAY)
            }
            "agenda" => (a, a + 14.0 * DAY),
            _ => {
                let back = ((date_of(a).get_day() + 6) % 7) as f64;
                (a - back * DAY, a - back * DAY + 7.0 * DAY)
            }
        }
    };
    let refresh = RwSignal::new(0u32);
    let events = LocalResource::new(move || {
        let ((from, to), _) = (range(), refresh.get());
        api::rpc_as::<Vec<CalendarEvent>>("memex.calendar.events", json!({"from": from as i64, "to": to as i64}))
    });
    let calendars = LocalResource::new(move || {
        let _ = refresh.get();
        api::rpc_as::<Vec<Calendar>>("memex.calendar.calendars", json!({}))
    });
    let open = RwSignal::new(None::<CalendarEvent>);
    let creating = RwSignal::new(false);
    let step = move |dir: f64| {
        let n = match mode.get_untracked() {
            "month" => {
                let d = date_of(anchor.get_untracked());
                d.set_date(1);
                d.set_month((d.get_month() as f64 + dir) as u32);
                anchor.set(midnight(d.get_time()));
                return;
            }
            "agenda" => 14.0,
            _ => 7.0,
        };
        anchor.update(|a| *a = midnight(*a + dir * n * DAY + DAY / 2.0));
    };
    view! {
        <main class="mx-auto max-w-6xl space-y-3 px-3 py-3">
            <div class="flex flex-wrap items-center gap-2">
                <button class=BUTTON_QUIET on:click=move |_| anchor.set(midnight(js_sys::Date::now()))>"Today"</button>
                <button class=BUTTON_QUIET on:click=move |_| step(-1.0)>"‹"</button>
                <button class=BUTTON_QUIET on:click=move |_| step(1.0)>"›"</button>
                <span class="font-medium">{move || {
                    let d = date_of(anchor.get());
                    String::from(d.to_locale_date_string("default", &js_sys::Object::from(js_sys::JSON::parse(r#"{"month":"long","year":"numeric"}"#).unwrap())))
                }}</span>
                <span class="flex-1"></span>
                {[("agenda", "Agenda"), ("week", "Week"), ("month", "Month")].into_iter().map(|(m, label)| view! {
                    <button class=move || format!("rounded-md px-2 py-1 text-sm {}", if mode.get() == m { "bg-zinc-200 dark:bg-zinc-800" } else { "text-zinc-500" }) on:click=move |_| mode.set(m)>{label}</button>
                }).collect_view()}
                <button class=BUTTON on:click=move |_| creating.set(true)>"New event"</button>
            </div>
            {move || calendars.get().and_then(Result::ok).map(|cs| view! {
                <div class="flex flex-wrap gap-3 text-xs">
                    {cs.into_iter().map(|c| {
                        let (acc, id) = (c.account.clone(), c.id.clone());
                        view! {
                            <label class="flex items-center gap-1">
                                <input type="checkbox" prop:checked=c.selected on:change=move |ev| {
                                    let (acc, id, on) = (acc.clone(), id.clone(), event_target_checked(&ev));
                                    spawn_local(async move { let _ = api::rpc("memex.calendar.select", json!({"account": acc, "id": id, "on": on})).await; refresh.update(|n| *n += 1); });
                                } />
                                <span class="h-2 w-2 rounded-full" style=format!("background:{}", if c.color.is_empty() { "#888".to_owned() } else { c.color.clone() })></span>
                                {c.summary.clone()}
                            </label>
                        }
                    }).collect_view()}
                </div>
            })}
            {move || creating.get().then(|| view! { <NewEvent creating refresh /> })}
            {move || open.get().map(|e| view! { <Detail e open refresh /> })}
            <Suspense fallback=|| view! { <p class="text-sm text-zinc-500">"Loading…"</p> }>
                {move || events.get().map(|r| match r {
                    Err(e) => view! { <ErrorBox message=format!("{e} (Connect a Google account in Settings → Mail & calendar.)") /> }.into_any(),
                    Ok(list) => {
                        let (from, to) = range();
                        let days: Vec<f64> = (0..((to - from) / DAY).round() as i64).map(|i| midnight(from + i as f64 * DAY + DAY / 2.0)).collect();
                        let on = |day: f64| -> Vec<CalendarEvent> {
                            list.iter().filter(|e| (e.start as f64) < day + DAY && (e.end as f64) > day && !(e.all_day && e.end as f64 <= day)).cloned().collect()
                        };
                        let chip = move |e: CalendarEvent| {
                            let e2 = e.clone();
                            let declined = e.my_status == "declined";
                            view! {
                                <button class=format!("block w-full truncate rounded px-1 py-0.5 text-left text-xs hover:bg-sky-100 dark:hover:bg-sky-950 {}", if declined { "line-through text-zinc-400" } else { "" })
                                    on:click=move |_| open.set(Some(e2.clone()))>
                                    {(!e.all_day).then(|| view! { <span class="text-zinc-500">{hm(e.start)}" "</span> })}
                                    {e.summary.clone()}
                                </button>
                            }
                        };
                        match mode.get() {
                            "month" => view! {
                                <div class="grid grid-cols-7 gap-px overflow-hidden rounded-lg border border-zinc-200 bg-zinc-200 text-xs dark:border-zinc-800 dark:bg-zinc-800">
                                    {days.into_iter().map(|d| {
                                        let today = d == midnight(js_sys::Date::now());
                                        view! {
                                            <div class="min-h-20 bg-white p-1 dark:bg-zinc-950">
                                                <div class=if today { "font-semibold text-sky-600" } else { "text-zinc-500" }>{date_of(d).get_date()}</div>
                                                {on(d).into_iter().take(4).map(chip).collect_view()}
                                            </div>
                                        }
                                    }).collect_view()}
                                </div>
                            }.into_any(),
                            "agenda" => view! {
                                <div class="space-y-3">
                                    {days.into_iter().filter_map(|d| {
                                        let evs = on(d);
                                        (!evs.is_empty()).then(|| view! {
                                            <div>
                                                <h3 class="text-xs font-medium uppercase text-zinc-500">{day_label(d)}</h3>
                                                {evs.into_iter().map(chip).collect_view()}
                                            </div>
                                        })
                                    }).collect_view()}
                                </div>
                            }.into_any(),
                            _ => view! {
                                <div class="grid gap-2 sm:grid-cols-7">
                                    {days.into_iter().map(|d| view! {
                                        <div class="min-h-24 rounded-lg border border-zinc-200 p-1 dark:border-zinc-800">
                                            <div class="mb-1 text-xs font-medium text-zinc-500">{day_label(d)}</div>
                                            {on(d).into_iter().map(chip).collect_view()}
                                        </div>
                                    }).collect_view()}
                                </div>
                            }.into_any(),
                        }
                    }
                })}
            </Suspense>
        </main>
    }
}

#[component]
fn Detail(e: CalendarEvent, open: RwSignal<Option<CalendarEvent>>, refresh: RwSignal<u32>) -> impl IntoView {
    let error = RwSignal::new(None::<String>);
    let ev = StoredValue::new(e.clone());
    let scope = RwSignal::new("this".to_owned());
    let rsvp = move |answer: &'static str| {
        let e = ev.get_value();
        spawn_local(async move {
            let r = api::rpc("memex.calendar.rsvp", json!({"account": e.account, "calendar_id": e.calendar_id, "event_id": e.event_id, "instance_start": e.instance_start, "response": answer})).await;
            match r {
                Ok(_) => {
                    open.set(None);
                    refresh.update(|n| *n += 1);
                }
                Err(x) => error.set(Some(x)),
            }
        });
    };
    let delete = move |_| {
        if !window().confirm_with_message("Delete this event? Attendees are told.").unwrap_or(false) {
            return;
        }
        let e = ev.get_value();
        let s = scope.get_untracked();
        spawn_local(async move {
            let r = api::rpc("memex.calendar.delete", json!({"account": e.account, "calendar_id": e.calendar_id, "event_id": e.event_id, "instance_start": e.instance_start, "scope": if e.instance_start.is_some() { s } else { "all".into() }})).await;
            match r {
                Ok(_) => {
                    open.set(None);
                    refresh.update(|n| *n += 1);
                }
                Err(x) => error.set(Some(x)),
            }
        });
    };
    let when = if e.all_day { "All day".to_owned() } else { format!("{} {}–{}", day_label(e.start as f64), hm(e.start), hm(e.end)) };
    view! {
        <section class="space-y-2 rounded-lg border border-zinc-300 p-3 shadow-sm dark:border-zinc-700">
            <div class="flex items-start gap-2">
                <div class="min-w-0 flex-1">
                    <h2 class="font-semibold">{e.summary.clone()}</h2>
                    <p class="text-sm text-zinc-500">{when}{(!e.location.is_empty()).then(|| format!(" · {}", e.location))}</p>
                </div>
                <button class=BUTTON_QUIET on:click=move |_| open.set(None)>"Close"</button>
            </div>
            {move || error.get().map(|x| view! { <ErrorBox message=x /> })}
            {(!e.conference.is_empty()).then(|| view! { <a class="text-sm text-sky-700 hover:underline dark:text-sky-400" href=crate::markdown::safe_href(&e.conference) target="_blank" rel="noopener">"Join the call ↗"</a> })}
            {(!e.description.is_empty()).then(|| view! { <pre class="max-h-48 overflow-auto whitespace-pre-wrap font-sans text-sm">{e.description.clone()}</pre> })}
            {(!e.attendees.is_empty()).then(|| view! {
                <ul class="text-sm">
                    {e.attendees.iter().map(|a| view! {
                        <li>{if a.name.is_empty() { a.email.clone() } else { a.name.clone() }}<span class="ml-1 text-xs text-zinc-500">{a.response.clone()}</span></li>
                    }).collect_view()}
                </ul>
            })}
            <div class="flex flex-wrap items-center gap-2">
                {(!e.my_status.is_empty()).then(|| view! {
                    <span class="text-xs text-zinc-500">"Going?"</span>
                    <button class=BUTTON_QUIET on:click=move |_| rsvp("accepted")>"Yes"</button>
                    <button class=BUTTON_QUIET on:click=move |_| rsvp("tentative")>"Maybe"</button>
                    <button class=BUTTON_QUIET on:click=move |_| rsvp("declined")>"No"</button>
                })}
                <span class="flex-1"></span>
                {e.instance_start.is_some().then(|| view! {
                    <select class=format!("{INPUT} w-auto py-1 text-xs") prop:value=move || scope.get() on:change=move |ev| scope.set(event_target_value(&ev))>
                        <option value="this">"This event"</option>
                        <option value="following">"This and following"</option>
                        <option value="all">"All events"</option>
                    </select>
                })}
                <button class="text-xs text-red-600 hover:underline" on:click=delete>"Delete"</button>
                {(!e.html_link.is_empty()).then(|| view! { <a class="text-xs text-sky-700 hover:underline dark:text-sky-400" href=crate::markdown::safe_href(&e.html_link) target="_blank" rel="noopener">"Open in Google ↗"</a> })}
            </div>
        </section>
    }
}

#[component]
fn NewEvent(creating: RwSignal<bool>, refresh: RwSignal<u32>) -> impl IntoView {
    let calendars = LocalResource::new(|| api::rpc_as::<Vec<Calendar>>("memex.calendar.calendars", json!({})));
    let (title, start, end, location, attendees, cal) = (
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
        RwSignal::new(String::new()),
    );
    let all_day = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let save = move |_| {
        let (s, e) = (js_sys::Date::parse(&start.get_untracked()), js_sys::Date::parse(&end.get_untracked()));
        if s.is_nan() || e.is_nan() || title.get_untracked().trim().is_empty() {
            error.set(Some("A title, a start and an end".into()));
            return;
        }
        let (account, calendar) = cal.get_untracked().split_once('|').map(|(a, c)| (a.to_owned(), c.to_owned())).unwrap_or_default();
        let who: Vec<String> = attendees.get_untracked().split(',').map(|x| x.trim().to_owned()).filter(|x| !x.is_empty()).collect();
        let body = json!({"account": account, "calendar_id": Some(calendar).filter(|c| !c.is_empty()), "summary": title.get_untracked(), "start": s as i64, "end": e as i64,
                          "all_day": all_day.get_untracked(), "location": location.get_untracked(), "attendees": who});
        spawn_local(async move {
            match api::rpc("memex.calendar.create", body).await {
                Ok(_) => {
                    creating.set(false);
                    refresh.update(|n| *n += 1);
                }
                Err(x) => error.set(Some(x)),
            }
        });
    };
    view! {
        <section class="space-y-2 rounded-lg border border-zinc-300 p-3 shadow-sm dark:border-zinc-700">
            {move || error.get().map(|x| view! { <ErrorBox message=x /> })}
            <div class="grid gap-2 sm:grid-cols-2">
                <input class=format!("{INPUT} py-1 text-sm sm:col-span-2") placeholder="Title" prop:value=move || title.get() on:input=move |ev| title.set(event_target_value(&ev)) />
                <input class=format!("{INPUT} py-1 text-sm") type="datetime-local" prop:value=move || start.get() on:input=move |ev| start.set(event_target_value(&ev)) />
                <input class=format!("{INPUT} py-1 text-sm") type="datetime-local" prop:value=move || end.get() on:input=move |ev| end.set(event_target_value(&ev)) />
                <input class=format!("{INPUT} py-1 text-sm") placeholder="Where" prop:value=move || location.get() on:input=move |ev| location.set(event_target_value(&ev)) />
                <input class=format!("{INPUT} py-1 text-sm") placeholder="Invite (addresses, comma-separated)" prop:value=move || attendees.get() on:input=move |ev| attendees.set(event_target_value(&ev)) />
                <select class=format!("{INPUT} py-1 text-sm") prop:value=move || cal.get() on:change=move |ev| cal.set(event_target_value(&ev))>
                    <option value="">"Calendar…"</option>
                    {move || calendars.get().and_then(Result::ok).unwrap_or_default().into_iter().filter(|c| c.writable).map(|c| view! { <option value=format!("{}|{}", c.account, c.id)>{c.summary.clone()}</option> }).collect_view()}
                </select>
                <label class="flex items-center gap-2 text-sm"><input type="checkbox" prop:checked=move || all_day.get() on:change=move |ev| all_day.set(event_target_checked(&ev)) />"All day"</label>
            </div>
            <div class="flex justify-end gap-2">
                <button class=BUTTON_QUIET on:click=move |_| creating.set(false)>"Cancel"</button>
                <button class=BUTTON on:click=save>"Create"</button>
            </div>
        </section>
    }
}
