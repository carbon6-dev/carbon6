//! Usage and budgets: tokens and cost by day, model, connection, label and
//! session, for the label filter's sessions; where each budget stands.

use c6_api::{BudgetStatus, UsageReport, UsageRow};
use leptos::prelude::*;

use crate::api;
use crate::util::{ErrorBox, INPUT};
use crate::workspace::{Go, View};

fn usd(x: f64) -> String {
    if x == 0.0 {
        "$0".into()
    } else if x < 0.01 {
        "<$0.01".into()
    } else {
        format!("${x:.2}")
    }
}

fn tokens(n: u64) -> String {
    match n {
        0..1_000 => n.to_string(),
        1_000..1_000_000 => format!("{:.1}k", n as f64 / 1e3),
        _ => format!("{:.1}M", n as f64 / 1e6),
    }
}

#[component]
pub fn UsagePage() -> impl IntoView {
    let filter = crate::labels::use_filter().0;
    let days = RwSignal::new(crate::util::storage_get("c6.usage.days").and_then(|d| d.parse().ok()).unwrap_or(30u32));
    Effect::new(move || crate::util::storage_set("c6.usage.days", &days.get().to_string()));
    let report = LocalResource::new(move || api::usage(days.get(), filter.get()));
    view! {
        <main class="mx-auto max-w-4xl space-y-4 px-4 py-4">
            <div class="flex flex-wrap items-center justify-between gap-2">
                <p class="text-xs text-zinc-500">"Tokens and cost of model calls in sessions with the filter's labels. Prices and budgets are in Settings."</p>
                <select class=format!("{INPUT} w-auto py-1 text-sm") prop:value=move || days.get().to_string()
                    on:change=move |ev| days.set(event_target_value(&ev).parse().unwrap_or(30))>
                    <option value="1">"Today"</option>
                    <option value="7">"Last 7 days"</option>
                    <option value="30">"Last 30 days"</option>
                    <option value="90">"Last 90 days"</option>
                </select>
            </div>
            <crate::labels::LabelFilterBar />
            <Suspense fallback=|| view! { <p class="text-sm text-zinc-500">"Loading…"</p> }>
                {move || report.get().map(|r| match r {
                    Ok(r) => view! { <Report r /> }.into_any(),
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                })}
            </Suspense>
        </main>
    }
}

#[component]
fn Report(r: UsageReport) -> impl IntoView {
    let t = r.total.clone();
    let max_day = r.by_day.iter().map(|d| d.cost).fold(0.0, f64::max);
    let max_calls = r.by_day.iter().map(|d| d.calls).max().unwrap_or(0);
    // Without prices, bars show calls.
    let by_cost = max_day > 0.0;
    view! {
        <div class="grid grid-cols-2 gap-3 sm:grid-cols-4">
            <Stat label="Cost" value=usd(t.cost) />
            <Stat label="Model calls" value=t.calls.to_string() />
            <Stat label="Input tokens" value=format!("{} (+{} cached)", tokens(t.input), tokens(t.cache_read)) />
            <Stat label="Output tokens" value=tokens(t.output) />
        </div>
        {(!r.unpriced_models.is_empty()).then(|| view! {
            <p class="rounded-md border border-amber-300 bg-amber-50 px-3 py-2 text-xs text-amber-900 dark:border-amber-900 dark:bg-amber-950 dark:text-amber-200">
                "No price for " {r.unpriced_models.join(", ")} ": those calls count as $0. Add prices in "
                <Go to=View::Settings { note: None } class="underline">"Settings → Prices"</Go> "."
            </p>
        })}
        {(!r.budgets.is_empty()).then(|| view! {
            <section class="space-y-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
                <h2 class="text-sm font-medium">"Budgets"</h2>
                {r.budgets.iter().cloned().map(|b| view! { <BudgetBar b /> }).collect_view()}
            </section>
        })}
        {(!r.by_day.is_empty()).then(|| view! {
            <section class="space-y-1 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
                <h2 class="text-sm font-medium">{if by_cost { "Cost per day" } else { "Model calls per day" }}</h2>
                <div class="flex h-28 items-end gap-0.5">
                    {r.by_day.iter().map(|d| {
                        let frac = if by_cost { d.cost / max_day } else { d.calls as f64 / max_calls.max(1) as f64 };
                        let tip = format!("{}: {} · {} calls", d.key, usd(d.cost), d.calls);
                        view! { <div class="min-w-[3px] flex-1 rounded-t bg-sky-500/80" style=format!("height:{}%", (frac * 100.0).max(2.0)) title=tip></div> }
                    }).collect_view()}
                </div>
                <div class="flex justify-between text-[10px] text-zinc-500">
                    <span>{r.by_day.first().map(|d| d.key.clone())}</span>
                    <span>{r.by_day.last().map(|d| d.key.clone())}</span>
                </div>
            </section>
        })}
        <div class="grid gap-3 lg:grid-cols-2">
            <Table title="By model" rows=r.by_model.clone() link=false />
            <Table title="By connection" rows=r.by_connection.clone() link=false />
            <Table title="By label" rows=r.by_label.clone() link=false />
            <Table title="Most expensive sessions" rows=r.top_sessions.clone() link=true />
        </div>
    }
}

#[component]
fn Stat(label: &'static str, value: String) -> impl IntoView {
    view! {
        <div class="rounded-lg border border-zinc-200 px-3 py-2 dark:border-zinc-800">
            <p class="text-xs text-zinc-500">{label}</p>
            <p class="text-lg font-semibold tabular-nums">{value}</p>
        </div>
    }
}

#[component]
fn BudgetBar(b: BudgetStatus) -> impl IntoView {
    let frac = (b.spent / b.limit).clamp(0.0, 1.0);
    let tone = if b.spent >= b.limit {
        "bg-red-500"
    } else if frac >= 0.8 {
        "bg-amber-500"
    } else {
        "bg-emerald-500"
    };
    let action = match b.action.as_str() {
        "block" => "blocks when used up",
        "notify" => "notifies when used up",
        _ => "off",
    };
    view! {
        <div class="space-y-1 text-sm">
            <div class="flex items-baseline justify-between gap-2">
                <span>{b.name.clone()} <span class="text-xs text-zinc-500">{format!(" · per {} · {action}", b.period)}</span></span>
                <span class="tabular-nums">{format!("{} of {}", usd(b.spent), usd(b.limit))}
                    {b.blocking.then(|| view! { <span class="ml-2 rounded bg-red-600 px-1.5 text-[10px] font-semibold text-white">"BLOCKING"</span> })}
                </span>
            </div>
            <div class="h-1.5 overflow-hidden rounded bg-zinc-200 dark:bg-zinc-800">
                <div class=format!("h-full {tone}") style=format!("width:{}%", frac * 100.0)></div>
            </div>
        </div>
    }
}

#[component]
fn Table(title: &'static str, rows: Vec<UsageRow>, link: bool) -> impl IntoView {
    view! {
        <section class="rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
            <h2 class="mb-2 text-sm font-medium">{title}</h2>
            {if rows.is_empty() {
                view! { <p class="text-xs text-zinc-500">"Nothing yet."</p> }.into_any()
            } else {
                view! {
                    <div class="overflow-x-auto"><table class="w-full text-xs">
                        <thead class="text-zinc-500"><tr>
                            <th class="pb-1 text-left font-normal"></th>
                            <th class="pb-1 text-right font-normal">"Calls"</th>
                            <th class="pb-1 text-right font-normal">"In"</th>
                            <th class="pb-1 text-right font-normal">"Out"</th>
                            <th class="pb-1 text-right font-normal">"Cost"</th>
                        </tr></thead>
                        <tbody>
                            {rows.into_iter().map(|r| {
                                let name = if r.name.is_empty() { r.key.clone() } else { r.name.clone() };
                                let cell = if link {
                                    view! { <Go to=View::session(r.key.clone()) class="hover:underline">{name}</Go> }.into_any()
                                } else {
                                    view! { <span class="font-mono">{name}</span> }.into_any()
                                };
                                view! {
                                    <tr class="border-t border-zinc-100 dark:border-zinc-800/60">
                                        <td class="max-w-[14rem] truncate py-1 pr-2">{cell}</td>
                                        <td class="py-1 text-right tabular-nums">{r.calls}</td>
                                        <td class="py-1 text-right tabular-nums">{tokens(r.input + r.cache_read)}</td>
                                        <td class="py-1 text-right tabular-nums">{tokens(r.output)}</td>
                                        <td class="py-1 text-right tabular-nums">{if r.calls == r.unpriced_calls { "—".into() } else { usd(r.cost) }}</td>
                                    </tr>
                                }
                            }).collect_view()}
                        </tbody>
                    </table></div>
                }.into_any()
            }}
        </section>
    }
}
