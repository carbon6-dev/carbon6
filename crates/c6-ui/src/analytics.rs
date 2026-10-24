//! Analytics (docs/analytics.md): agent, model and tool time, cost, and
//! what the work took of you, per day and by model, tool and a label key.

use c6_api::{ActivityRow, AnalyticsReport, ToolRow};
use leptos::prelude::*;

use crate::api;
use crate::util::{ErrorBox, INPUT, storage_get, storage_set};
use crate::workspace::{Go, View};

/// A duration for people: 3h 20m, 12m, 45s.
fn dur(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0 if ms > 0 => format!("{ms}ms"),
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        _ => format!("{}h {}m", s / 3600, s % 3600 / 60),
    }
}

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

fn pct(part: u64, of: u64) -> String {
    if of == 0 { "–".into() } else { format!("{:.0}%", part as f64 * 100.0 / of as f64) }
}

#[component]
pub fn AnalyticsPage() -> impl IntoView {
    let filter = crate::labels::use_filter().0;
    let live = api::live();
    let days = RwSignal::new(storage_get("c6.analytics.days").and_then(|d| d.parse().ok()).unwrap_or(30u32));
    let by = RwSignal::new(storage_get("c6.analytics.by").unwrap_or_else(|| "repo".into()));
    Effect::new(move || storage_set("c6.analytics.days", &days.get().to_string()));
    Effect::new(move || storage_set("c6.analytics.by", &by.get()));
    let report = LocalResource::new(move || api::analytics(days.get(), filter.get(), by.get()));
    // Keys to group by: the automatic ones and those of your labels.
    let keys = Memo::new(move |_| {
        let mut keys: Vec<String> = ["repo", "host", "worker", "folder", "model"].map(str::to_owned).to_vec();
        live.sessions.with(|ss| {
            for l in ss.iter().flat_map(|s| s.labels.iter()) {
                if let Some((k, _)) = l.split_once(':')
                    && !keys.iter().any(|x| x == k)
                {
                    keys.push(k.to_owned());
                }
            }
        });
        keys
    });
    view! {
        <main class="viz mx-auto max-w-5xl space-y-4 px-4 py-4">
            <div class="flex flex-wrap items-center justify-between gap-2">
                <p class="text-xs text-zinc-500">"Who worked when, what it cost, and what it took of you, for sessions with the filter's labels."</p>
                <select class=format!("{INPUT} w-auto! py-1 text-sm") prop:value=move || days.get().to_string()
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
                    Ok(r) => view! { <Report r by keys /> }.into_any(),
                    Err(e) => view! { <ErrorBox message=e /> }.into_any(),
                })}
            </Suspense>
        </main>
    }
}

#[component]
fn Report(r: AnalyticsReport, by: RwSignal<String>, keys: Memo<Vec<String>>) -> impl IntoView {
    let t = r.total.clone();
    let hours = |ms: u64| ms as f64 / 3_600_000.0;
    let days: Vec<String> = r.by_day.iter().map(|d| d.key.clone()).collect();
    let agent = vec![
        ("Model", "var(--series-1)", r.by_day.iter().map(|d| hours(d.model_ms)).collect()),
        ("Tools", "var(--series-2)", r.by_day.iter().map(|d| hours(d.tool_ms)).collect()),
        ("Other", "var(--series-3)", r.by_day.iter().map(|d| hours(d.agent_ms.saturating_sub(d.model_ms + d.tool_ms))).collect()),
    ];
    let you = vec![
        ("New tasks", "var(--series-1)", r.by_day.iter().map(|d| d.tasks as f64).collect()),
        ("Corrections", "var(--series-2)", r.by_day.iter().map(|d| d.steers as f64).collect()),
        ("Answers", "var(--series-3)", r.by_day.iter().map(|d| d.answers as f64).collect()),
    ];
    let priced = t.cost > 0.0;
    let spend = if priced {
        vec![("Cost", "var(--series-1)", r.by_day.iter().map(|d| d.cost).collect())]
    } else {
        vec![("Model calls", "var(--series-1)", r.by_day.iter().map(|d| d.calls as f64).collect())]
    };
    let per_message = t.agent_ms.checked_div(t.messages).map_or_else(|| "–".into(), dur);
    view! {
        <div class="grid grid-cols-2 gap-3 sm:grid-cols-4">
            <Stat label="Agent time" value=dur(t.agent_ms) note=format!("{} turns, {} failed", t.turns, pct(t.turns_failed, t.turns)) />
            <Stat label="Agent time per message of yours" value=per_message note=format!("{} messages, {} corrections", t.messages, t.steers) />
            <Stat label="Agents at once (peak)" value=t.peak_agents.to_string() note=format!("{} sessions", t.sessions) />
            <Stat label="Work waiting on you (median)" value=if r.waits.replies == 0 { "–".into() } else { dur(r.waits.reply_median_ms) }
                note=format!("90% within {}", dur(r.waits.reply_p90_ms)) />
            <Stat label="Cost" value=usd(t.cost) note=format!("{} model calls", t.calls) />
            <Stat label="Tokens" value=format!("{} in · {} out", tokens(t.input + t.cache_read), tokens(t.output)) note=format!("{} of input cached", pct(t.cache_read, t.input + t.cache_read)) />
            <Stat label="Model time" value=dur(t.model_ms) note=format!("{} of agent time", pct(t.model_ms, t.agent_ms)) />
            <Stat label="Tool time" value=dur(t.tool_ms) note=format!("{} of agent time", pct(t.tool_ms, t.agent_ms)) />
        </div>
        {(!r.unpriced_models.is_empty()).then(|| view! {
            <p class="rounded-md border border-amber-300 bg-amber-50 px-3 py-2 text-xs text-amber-900 dark:border-amber-900 dark:bg-amber-950 dark:text-amber-200">
                "No price for " {r.unpriced_models.join(", ")} ": those calls count as $0. Add prices in "
                <Go to=View::Settings { note: None, section: "spending".into() } class="underline">"Settings → Prices"</Go> "."
            </p>
        })}
        <div class="grid gap-3 lg:grid-cols-2">
            <DayChart title="Agent time per day" days=days.clone() series=agent fmt=|h| dur((h * 3_600_000.0) as u64) />
            <DayChart title="You, per day" days=days.clone() series=you fmt=|n| format!("{n:.0}") />
            <DayChart title=if priced { "Cost per day" } else { "Model calls per day (no prices)" } days=days series=spend
                fmt=move |x| if priced { usd(x) } else { format!("{x:.0}") } />
            <Waits w=r.waits.clone() />
        </div>
        <Models rows=r.by_model.clone() />
        <Tools rows=r.by_tool.clone() />
        <Groups rows=r.by_group.clone() by keys />
    }
}

#[component]
fn Stat(label: &'static str, value: String, note: String) -> impl IntoView {
    let tip = note.clone();
    view! {
        <div class="rounded-lg border border-zinc-200 px-3 py-2 dark:border-zinc-800">
            <p class="text-xs text-zinc-500">{label}</p>
            <p class="text-lg font-semibold tabular-nums">{value}</p>
            <p class="truncate text-[11px] text-zinc-500" title=tip>{note}</p>
        </div>
    }
}

fn card(title: &'static str, body: impl IntoView) -> impl IntoView {
    view! {
        <section class="space-y-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
            <h2 class="text-sm font-medium">{title}</h2>
            {body}
        </section>
    }
}

/// Stacked bars, one per day (HTML, so it stays sharp at any width); hover
/// shows the day's values, and the table has them all.
#[component]
fn DayChart(
    title: &'static str,
    days: Vec<String>,
    series: Vec<(&'static str, &'static str, Vec<f64>)>,
    fmt: impl Fn(f64) -> String + Clone + Send + Sync + 'static,
) -> impl IntoView {
    let n = days.len();
    let at = |v: &Vec<f64>, i: usize| v.get(i).copied().unwrap_or(0.0);
    let max = (0..n).map(|i| series.iter().map(|s| at(&s.2, i)).sum::<f64>()).fold(0.0, f64::max);
    let label = |d: &str| d.get(5..).unwrap_or(d).replace('-', "/");
    let legend = (series.len() > 1).then(|| {
        series
            .iter()
            .map(|(name, color, _)| {
                view! {
                    <span class="inline-flex items-center gap-1">
                        <span class="inline-block size-2.5 rounded-sm" style=format!("background:{color}")></span>
                        {*name}
                    </span>
                }
            })
            .collect_view()
    });
    let columns = (0..n)
        .map(|i| {
            let tip = std::iter::once(days[i].clone())
                .chain(series.iter().map(|(name, _, v)| format!("{name}: {}", fmt(at(v, i)))))
                .collect::<Vec<_>>()
                .join("\n");
            let present: Vec<usize> = (0..series.len()).filter(|&k| at(&series[k].2, i) > 0.0).collect();
            let segs = present
                .iter()
                .enumerate()
                .map(|(j, &k)| {
                    let h = if max > 0.0 { at(&series[k].2, i) / max * 100.0 } else { 0.0 };
                    // The data end is rounded; a 2px gap separates segments.
                    let round = if j + 1 == present.len() { "rounded-t-[4px]" } else { "" };
                    view! { <div class=round style=format!("height:{h:.2}%; min-height:1px; background:{}", series[k].1)></div> }
                })
                .collect_view();
            view! {
                // The whole column answers hover, not just the bar.
                <div class="flex h-full min-w-0 flex-1 flex-col-reverse gap-[2px] hover:opacity-80" title=tip>{segs}</div>
            }
        })
        .collect_view();
    let ticks: Vec<usize> = if n <= 2 { (0..n).collect() } else { vec![0, n / 2, n - 1] };
    let body = view! {
        {legend.map(|l| view! { <div class="flex flex-wrap gap-3 text-xs text-zinc-600 dark:text-zinc-400">{l}</div> })}
        <div class="flex gap-2 text-[11px] text-zinc-500">
            <div class="flex h-36 shrink-0 flex-col justify-between text-right tabular-nums">
                <span>{fmt(max)}</span>
                <span>"0"</span>
            </div>
            <div class="min-w-0 flex-1">
                <div class="relative flex h-36 items-stretch gap-[2px] border-b border-zinc-300 dark:border-zinc-700" role="img" aria-label=title>
                    <div class="pointer-events-none absolute inset-x-0 top-0 border-t border-dashed border-zinc-200 dark:border-zinc-800"></div>
                    {columns}
                </div>
                <div class="relative h-4">
                    {ticks.into_iter().map(|i| {
                        let left = (i as f64 + 0.5) / n.max(1) as f64 * 100.0;
                        view! { <span class="absolute -translate-x-1/2 whitespace-nowrap" style=format!("left:{left:.2}%")>{label(&days[i])}</span> }
                    }).collect_view()}
                </div>
            </div>
        </div>
        <details class="text-xs text-zinc-500">
            <summary class="cursor-pointer">"Table"</summary>
            <table class="mt-1 w-full tabular-nums">
                <thead><tr>
                    <th class="text-left font-normal">"Day"</th>
                    {series.iter().map(|(name, _, _)| view! { <th class="text-right font-normal">{*name}</th> }).collect_view()}
                </tr></thead>
                <tbody class="text-zinc-700 dark:text-zinc-300">
                    {days.iter().enumerate().rev().map(|(i, d)| view! {
                        <tr>
                            <td>{d.clone()}</td>
                            {series.iter().map(|(_, _, v)| view! { <td class="text-right">{fmt(at(v, i))}</td> }).collect_view()}
                        </tr>
                    }).collect_view()}
                </tbody>
            </table>
        </details>
    };
    card(title, body)
}

#[component]
fn Waits(w: c6_api::Waits) -> impl IntoView {
    let row = |what: &'static str, n: u64, median: u64, p90: u64| {
        view! {
            <tr class="border-t border-zinc-100 dark:border-zinc-800">
                <td class="py-1.5">{what}</td>
                <td class="whitespace-nowrap pl-3 text-right tabular-nums">{n}</td>
                <td class="whitespace-nowrap pl-3 text-right tabular-nums">{if n == 0 { "–".into() } else { dur(median) }}</td>
                <td class="whitespace-nowrap pl-3 text-right tabular-nums">{if n == 0 { "–".into() } else { dur(p90) }}</td>
            </tr>
        }
    };
    card(
        "Waiting on you",
        view! {
            <table class="w-full text-sm">
                <thead class="text-xs text-zinc-500"><tr>
                    <th class="text-left font-normal"></th><th class="pl-3 text-right font-normal">"Times"</th>
                    <th class="pl-3 text-right font-normal">"Median"</th><th class="pl-3 text-right font-normal">"90%"</th>
                </tr></thead>
                <tbody>
                    {row("Your reply to a finished turn", w.replies, w.reply_median_ms, w.reply_p90_ms)}
                    {row("Your answer to an approval or question", w.answers, w.answer_median_ms, w.answer_p90_ms)}
                </tbody>
            </table>
            <p class="text-xs text-zinc-500">"How long sessions sat waiting for you: what to shorten to get more done."</p>
        },
    )
}

/// A thin bar for a share of the largest value, beside the number.
fn share(v: f64, max: f64) -> impl IntoView {
    let w = if max > 0.0 { (v / max * 100.0).clamp(0.0, 100.0) } else { 0.0 };
    view! {
        <div class="h-1.5 w-full rounded-full bg-zinc-100 dark:bg-zinc-800">
            <div class="h-1.5 rounded-full" style=format!("width:{w:.1}%; background: var(--series-1)")></div>
        </div>
    }
}

const TH: &str = "py-1 text-right font-normal";
const TD: &str = "whitespace-nowrap py-1.5 pl-3 text-right tabular-nums";

#[component]
fn Models(rows: Vec<ActivityRow>) -> impl IntoView {
    let max = rows.iter().map(|r| r.model_ms).max().unwrap_or(0) as f64;
    card(
        "Models",
        view! {
            <div class="overflow-x-auto">
                <table class="w-full min-w-[36rem] text-sm">
                    <thead class="text-xs text-zinc-500"><tr>
                        <th class="py-1 text-left font-normal">"Model"</th><th class="w-32"></th><th class=TH>"Time"</th>
                        <th class=TH>"Calls"</th><th class=TH>"Per call"</th><th class=TH>"Output/s"</th>
                        <th class=TH>"Tokens in · out"</th><th class=TH>"Cost"</th>
                    </tr></thead>
                    <tbody>
                        {rows.into_iter().map(|r| view! {
                            <tr class="border-t border-zinc-100 dark:border-zinc-800">
                                <td class="max-w-48 truncate py-1.5 font-mono text-xs" title=r.key.clone()>{r.key.clone()}</td>
                                <td class="px-2">{share(r.model_ms as f64, max)}</td>
                                <td class=TD>{dur(r.model_ms)}</td>
                                <td class=TD>{r.calls}</td>
                                <td class=TD>{r.model_ms.checked_div(r.calls).map_or_else(|| "–".into(), dur)}</td>
                                <td class=TD>{if r.model_ms == 0 { "–".into() } else { format!("{:.0} tok", r.output as f64 * 1000.0 / r.model_ms as f64) }}</td>
                                <td class=TD>{format!("{} · {}", tokens(r.input + r.cache_read), tokens(r.output))}</td>
                                <td class=TD>{usd(r.cost)}</td>
                            </tr>
                        }).collect_view()}
                    </tbody>
                </table>
            </div>
        },
    )
}

#[component]
fn Tools(rows: Vec<ToolRow>) -> impl IntoView {
    let max = rows.iter().map(|r| r.ms).max().unwrap_or(0) as f64;
    card(
        "Tools",
        view! {
            <div class="overflow-x-auto">
                <table class="w-full min-w-[30rem] text-sm">
                    <thead class="text-xs text-zinc-500"><tr>
                        <th class="py-1 text-left font-normal">"Tool"</th><th class="w-32"></th><th class=TH>"Time"</th>
                        <th class=TH>"Calls"</th><th class=TH>"Median"</th><th class=TH>"Failed"</th>
                    </tr></thead>
                    <tbody>
                        {rows.into_iter().map(|r| view! {
                            <tr class="border-t border-zinc-100 dark:border-zinc-800">
                                <td class="py-1.5 font-mono text-xs">{r.key.clone()}</td>
                                <td class="px-2">{share(r.ms as f64, max)}</td>
                                <td class=TD>{dur(r.ms)}</td>
                                <td class=TD>{r.calls}</td>
                                <td class=TD>{dur(r.median_ms)}</td>
                                <td class=TD>{pct(r.failed, r.calls)}</td>
                            </tr>
                        }).collect_view()}
                    </tbody>
                </table>
            </div>
        },
    )
}

#[component]
fn Groups(rows: Vec<ActivityRow>, by: RwSignal<String>, keys: Memo<Vec<String>>) -> impl IntoView {
    let max = rows.iter().map(|r| r.agent_ms).max().unwrap_or(0) as f64;
    let key = by.get_untracked();
    view! {
        <section class="space-y-2 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
            <div class="flex items-center gap-2">
                <h2 class="text-sm font-medium">"By"</h2>
                <select class=format!("{INPUT} w-auto! py-0.5 text-sm") prop:value=move || by.get() on:change=move |ev| by.set(event_target_value(&ev))>
                    {move || keys.get().into_iter().map(|k| { let v = k.clone(); view! { <option value=v>{k}</option> } }).collect_view()}
                </select>
            </div>
            <div class="overflow-x-auto">
                <table class="w-full min-w-[36rem] text-sm">
                    <thead class="text-xs text-zinc-500"><tr>
                        <th class="py-1 text-left font-normal">{key.clone()}</th><th class="w-32"></th><th class=TH>"Agent time"</th>
                        <th class=TH>"Sessions"</th><th class=TH>"Turns"</th><th class=TH>"Your messages"</th>
                        <th class=TH>"Corrections per task"</th><th class=TH>"Cost"</th>
                    </tr></thead>
                    <tbody>
                        {rows.into_iter().map(|r| {
                            let name = if r.key.is_empty() { view! { <span class="italic text-zinc-500">{format!("no {key}")}</span> }.into_any() }
                                else { view! { <span>{r.key.clone()}</span> }.into_any() };
                            view! {
                                <tr class="border-t border-zinc-100 dark:border-zinc-800">
                                    <td class="max-w-56 truncate py-1.5 font-mono text-xs" title=r.key.clone()>{name}</td>
                                    <td class="px-2">{share(r.agent_ms as f64, max)}</td>
                                    <td class=TD>{dur(r.agent_ms)}</td>
                                    <td class=TD>{r.sessions}</td>
                                    <td class=TD>{r.turns}</td>
                                    <td class=TD>{r.messages}</td>
                                    <td class=TD>{if r.tasks == 0 { "–".into() } else { format!("{:.1}", r.steers as f64 / r.tasks as f64) }}</td>
                                    <td class=TD>{usd(r.cost)}</td>
                                </tr>
                            }
                        }).collect_view()}
                    </tbody>
                </table>
            </div>
        </section>
    }
}
