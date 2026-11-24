//! Machine inventory and lightweight, timestamp-shaped resource charts.

use std::time::Duration;

use c6_api::machines::{Machine, MachineDiskScan, MachineSample};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::{
    api,
    workspace::{Go, View},
};

fn now_ms() -> i64 {
    js_sys::Date::now() as i64
}

fn sample_stale(online: bool, at_ms: Option<i64>, now_ms: i64, sample_secs: u64) -> bool {
    let threshold_ms = sample_secs.max(1).saturating_mul(3000).max(60_000).min(i64::MAX as u64) as i64;
    !online || at_ms.is_some_and(|at| now_ms.saturating_sub(at) > threshold_ms)
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut n = n as f64;
    let mut u = 0;
    while n >= 1024.0 && u < UNITS.len() - 1 {
        n /= 1024.0;
        u += 1;
    }
    format!("{n:.1} {}", UNITS[u])
}

fn percent(used: u64, total: u64) -> f64 {
    if total == 0 { f64::NAN } else { used as f64 / total as f64 * 100.0 }
}

fn fullest(s: &MachineSample) -> f64 {
    s.disks.iter().filter(|d| d.total > 0).map(|d| percent(d.used, d.total)).reduce(f64::max).unwrap_or(f64::NAN)
}

fn update_summary(update: &serde_json::Value) -> String {
    if let Some(s) = update.as_str() {
        return s.to_owned();
    }
    if let Some(head) =
        update.get("machines").and_then(serde_json::Value::as_array).and_then(|ms| ms.iter().find(|m| m["machine"] == "head"))
    {
        if head.get("state").and_then(serde_json::Value::as_str).is_some_and(|s| !s.is_empty()) {
            return update_summary(head);
        }
        if let Some(error) = update.get("check_error").and_then(serde_json::Value::as_str).filter(|s| !s.is_empty()) {
            return format!("update check failed: {error}");
        }
        if let Some(release) = update.get("releases").and_then(serde_json::Value::as_array).and_then(|rs| rs.first()) {
            return format!("release {}", update_summary(release));
        }
        return "no pending head update".into();
    }
    let parts: Vec<_> = ["state", "version", "detail"]
        .into_iter()
        .filter_map(|key| update.get(key).and_then(serde_json::Value::as_str))
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() { "no update activity reported".into() } else { parts.join(" · ") }
}

fn oom_markers(samples: &[MachineSample]) -> Vec<i64> {
    samples
        .windows(2)
        .filter_map(|s| match (s[0].oom_kills, s[1].oom_kills) {
            (Some(a), Some(b)) if b > a => Some(s[1].at_ms),
            _ => None,
        })
        .collect()
}

fn disk_series(samples: &[MachineSample], mount: &str) -> Vec<(i64, f64)> {
    samples
        .iter()
        .filter_map(|s| s.disks.iter().find(|d| d.mount == mount && d.total > 0).map(|d| (s.at_ms, percent(d.used, d.total))))
        .collect()
}

/// Sort timestamps, discard non-finite points and share one non-degenerate
/// time/value domain. Missing telemetry is never represented as a zero sample.
fn chart_path(points: &[(i64, f64)], start: i64, end: i64, ceiling: f64) -> String {
    let mut points: Vec<_> = points.iter().copied().filter(|(t, y)| *t >= start && *t <= end && y.is_finite()).collect();
    points.sort_by_key(|p| p.0);
    let span = end.saturating_sub(start).max(1) as f64;
    let max = points.iter().map(|p| p.1).fold(ceiling.max(1.0), f64::max);
    // Permit downsampled spacing without implying continuity across hours
    // offline. Short ranges have a fifteen-minute minimum gap threshold.
    let gap = (end.saturating_sub(start) / 100).max(900_000);
    points
        .iter()
        .enumerate()
        .map(|(i, (t, y))| {
            let x = t.saturating_sub(start) as f64 / span * 300.0;
            let y = 76.0 - y.max(0.0) / max * 72.0;
            format!("{}{x:.2},{y:.2}", if i == 0 || t.saturating_sub(points[i - 1].0) > gap { "M" } else { " L" })
        })
        .collect()
}

#[component]
fn Chart(
    label: String,
    points: Vec<(i64, f64)>,
    start: i64,
    end: i64,
    #[prop(default = 100.0)] ceiling: f64,
    #[prop(default = Vec::new())] oom: Vec<i64>,
) -> impl IntoView {
    let path = chart_path(&points, start, end, ceiling);
    let mut valid: Vec<_> = points.iter().filter(|(t, v)| *t >= start && *t <= end && v.is_finite()).collect();
    valid.sort_by_key(|p| p.0);
    let latest = valid.last().map(|p| format!("{:.1}", p.1)).unwrap_or_else(|| "unknown".into());
    let domain = valid.iter().map(|p| p.1).fold(ceiling.max(1.0), f64::max);
    let gap = (end.saturating_sub(start) / 100).max(900_000);
    let dots: Vec<_> = valid
        .iter()
        .enumerate()
        .filter(|(i, p)| {
            (*i == 0 || p.0.saturating_sub(valid[*i - 1].0) > gap) && (*i + 1 == valid.len() || valid[*i + 1].0.saturating_sub(p.0) > gap)
        })
        .map(|(_, (t, v))| {
            (
                (t.saturating_sub(start) as f64 / end.saturating_sub(start).max(1) as f64 * 300.0).to_string(),
                (76.0 - v.max(0.0) / domain * 72.0).to_string(),
            )
        })
        .collect();
    let title = format!("{label}: {latest}");
    view! {
        <figure class="min-w-0 rounded-lg border border-zinc-200 p-3 dark:border-zinc-800">
            <figcaption class="flex justify-between gap-2 text-xs text-muted"><span class="min-w-0 break-all">{label}</span><span class="shrink-0 tabular-nums">{latest}</span></figcaption>
            <svg viewBox="0 0 300 80" role="img" aria-label=title class="mt-2 h-20 w-full overflow-visible text-sky-600 dark:text-sky-400" preserveAspectRatio="none">
                <line x1="0" y1="76" x2="300" y2="76" stroke="currentColor" opacity="0.15" />
                <path d=path fill="none" stroke="currentColor" stroke-width="2" vector-effect="non-scaling-stroke" />
                {dots.into_iter().map(|(x,y)| view! { <circle cx=x cy=y r="2" fill="currentColor" /> }).collect_view()}
                {oom.into_iter().filter(|t| *t >= start && *t <= end).map(|t| {
                    let x = ((t - start) as f64 / (end - start).max(1) as f64 * 300.0).to_string();
                    view! { <line x1=x.clone() x2=x y1="0" y2="76" stroke="currentColor" class="text-red-600 dark:text-red-400" stroke-dasharray="3 3"><title>"New OOM kill"</title></line> }
                }).collect_view()}
                {valid.is_empty().then(|| view! { <text x="150" y="42" text-anchor="middle" fill="currentColor" font-size="12">"No samples yet"</text> })}
            </svg>
            <div class="mt-1 flex justify-between text-[10px] text-muted"><span>{format!("0–{domain:.0}")}</span><span>"latest →"</span></div>
        </figure>
    }
}

#[component]
pub fn Machines() -> impl IntoView {
    let machines = LocalResource::new(move || {
        api::realm().get();
        api::machines()
    });
    let selected = RwSignal::new("head".to_string());
    let tick = set_interval_with_handle(move || machines.refetch(), Duration::from_secs(15)).ok();
    on_cleanup(move || {
        if let Some(t) = tick {
            t.clear();
        }
    });
    view! {
        <section class="space-y-4">
            <div><h1 class="text-lg font-semibold">"Machines"</h1><p class="text-sm text-muted">"The head and workers running carbon6. Refreshes every 15 seconds."</p></div>
            <Suspense fallback=|| view! { <p class="text-sm text-muted">"Loading machines…"</p> }>
                {move || machines.get().map(|r| match r {
                    Err(e) => view! { <crate::util::ErrorBox message=e /> }.into_any(),
                    Ok(mut ms) => {
                        ms.sort_by_key(|m| (m.role != "head", m.name.clone()));
                        view! { <div class="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">{ms.into_iter().map(|m| view! { <MachineCard machine=m selected /> }).collect_view()}</div> }.into_any()
                    }
                })}
            </Suspense>
            {move || view! { <MachineDetails machine_id=selected.get() /> }}
        </section>
    }
}

#[component]
fn MachineCard(machine: Machine, selected: RwSignal<String>) -> impl IntoView {
    let id = machine.id.clone();
    let stale = sample_stale(machine.online, machine.last_sample.as_ref().map(|s| s.at_ms), now_ms(), machine.sample_secs);
    let sampled = machine
        .last_sample
        .as_ref()
        .map(|s| format!("Sample {}", crate::when::ago(now_ms() - s.at_ms)))
        .unwrap_or_else(|| "No telemetry received".into());
    let history_id = machine.id.clone();
    let until = now_ms();
    let history = LocalResource::new(move || api::machine_samples(history_id.clone(), until - 3_600_000, until));
    let metrics = machine
        .last_sample
        .as_ref()
        .map(|s| {
            vec![
                ("CPU", format!("{:.0}% · {} cores", s.cpu_percent, s.cpu_count), s.cpu_percent),
                ("Memory", format!("{} / {}", bytes(s.memory_used), bytes(s.memory_total)), percent(s.memory_used, s.memory_total)),
                (
                    "Fullest disk",
                    s.disks
                        .iter()
                        .filter(|d| d.total > 0)
                        .max_by(|a, b| percent(a.used, a.total).total_cmp(&percent(b.used, b.total)))
                        .map(|d| format!("{:.0}% · {}", percent(d.used, d.total), d.mount))
                        .unwrap_or_else(|| "unknown".into()),
                    fullest(s),
                ),
            ]
        })
        .unwrap_or_default();
    view! {
        <button class="min-w-0 space-y-3 rounded-lg border border-zinc-200 bg-white p-4 text-left hover:border-sky-500 aria-pressed:border-sky-500 aria-pressed:bg-sky-50 dark:border-zinc-800 dark:bg-zinc-900 dark:hover:border-sky-500 dark:aria-pressed:bg-sky-950"
            data-machine-card=machine.id.clone()
            aria-pressed=move || selected.get() == id
            on:click=move |_| selected.set(machine.id.clone())>
            <div class="flex min-w-0 items-center gap-2"><span class=if machine.online { "h-2 w-2 shrink-0 rounded-full bg-emerald-500" } else { "h-2 w-2 shrink-0 rounded-full bg-zinc-400" }></span><span class="min-w-0 break-words font-medium">{machine.name}</span><span class="ml-auto text-xs text-muted">{machine.role}</span></div>
            <p class="text-xs text-muted">{if machine.online { "online" } else { "offline" }}" · "{machine.version.unwrap_or_else(|| "version unknown".into())}</p>
            <p class="break-all text-xs text-muted">{machine.hostname}</p>
            {(!machine.warnings.is_empty()).then(|| view! { <span class="inline-flex rounded-full bg-amber-100 px-2 py-0.5 text-xs font-medium text-amber-900 dark:bg-amber-950 dark:text-amber-300" role="status">{format!("⚠ {} warning{}", machine.warnings.len(), if machine.warnings.len() == 1 { "" } else { "s" })}</span> })}
            <div class="space-y-2">{metrics.into_iter().map(|(label, value, _)| view! {
                <div><div class="flex justify-between gap-2 text-xs"><span class="shrink-0 text-muted">{label}</span><span class="min-w-0 break-all text-right">{value}</span></div><svg viewBox="0 0 300 80" class="mt-1 h-6 w-full text-sky-600 dark:text-sky-400" role="img" aria-label=format!("{label} over the last hour") preserveAspectRatio="none"><path d=move || {
                    let ss = history.get().and_then(Result::ok).unwrap_or_default();
                    let points: Vec<_> = ss.iter().map(|s| (s.at_ms, match label { "CPU" => s.cpu_percent, "Memory" => percent(s.memory_used, s.memory_total), _ => fullest(s) })).collect();
                    chart_path(&points, until - 3_600_000, until, 100.0)
                } fill="none" stroke="currentColor" stroke-width="1.5" vector-effect="non-scaling-stroke" /></svg></div>
            }).collect_view()}</div>
            <p class=if stale { "text-xs text-amber-700 dark:text-amber-400" } else { "text-xs text-muted" }>{sampled}{stale.then_some(" · stale / offline")}</p>
            {machine.warnings.into_iter().map(|w| view! { <p class="text-xs text-amber-700 dark:text-amber-400">{w}</p> }).collect_view()}
        </button>
    }
}

#[component]
pub fn MachineDetails(machine_id: String) -> impl IntoView {
    let machine_id = StoredValue::new(machine_id);
    let range = RwSignal::new(3_600_000i64);
    let end = RwSignal::new(now_ms());
    let samples = LocalResource::new(move || {
        api::realm().get();
        api::machine_samples(machine_id.get_value(), end.get() - range.get(), end.get())
    });
    let inventory = LocalResource::new(move || {
        api::realm().get();
        api::machines()
    });
    let tick = set_interval_with_handle(
        move || {
            end.set(now_ms());
            inventory.refetch();
        },
        Duration::from_secs(15),
    )
    .ok();
    on_cleanup(move || {
        if let Some(t) = tick {
            t.clear();
        }
    });
    view! {
        <section data-machine-panel=machine_id.get_value() class="min-w-0 space-y-4 rounded-xl border border-zinc-200 p-3 sm:p-4 dark:border-zinc-800">
            <div class="flex flex-wrap items-center justify-between gap-3"><h2 class="font-semibold">"Machine resources"</h2><div class="flex gap-1" role="group" aria-label="History range">{[(3_600_000, "1h"), (86_400_000, "24h"), (604_800_000, "7d")].into_iter().map(|(ms, label)| view! {
                <button class="rounded border border-zinc-200 px-3 py-1 text-xs aria-pressed:bg-zinc-200 dark:border-zinc-700 dark:aria-pressed:bg-zinc-700" aria-pressed=move || range.get() == ms on:click=move |_| { range.set(ms); end.set(now_ms()); }>{label}</button>
            }).collect_view()}</div></div>
            <Suspense fallback=|| ()>{move || inventory.get().map(|r| match r {
                Ok(ms) => ms.into_iter().find(|m| m.id == machine_id.get_value()).map(|m| view! { <MachineInfo machine=m /> }).into_any(),
                Err(e) => view! { <crate::util::ErrorBox message=e /> }.into_any(),
            })}</Suspense>
            <Suspense fallback=|| view! { <p class="text-sm text-muted">"Loading history…"</p> }>{move || samples.get().map(|r| match r {
                Ok(ss) => {
                    let current = inventory.get().and_then(Result::ok).and_then(|ms| ms.into_iter().find(|m| m.id == machine_id.get_value())).and_then(|m| m.last_sample);
                    view! { <History samples=ss current start=end.get() - range.get() end=end.get() /> }.into_any()
                },
                Err(e) => view! { <crate::util::ErrorBox message=e /> }.into_any(),
            })}</Suspense>
            <DiskScan machine_id=machine_id.get_value() />
        </section>
    }
}

#[component]
fn MachineInfo(machine: Machine) -> impl IntoView {
    let head = machine.role == "head";
    view! {
        <div class="space-y-2 text-sm"><div class="flex flex-wrap items-center gap-2"><strong class="break-all">{machine.name}</strong><span class="text-muted">{machine.role}" · "{if machine.online { "online" } else { "offline" }}</span>
            {(!head).then(|| view! { <Go to=View::Worker { id: machine.id.clone() } class="text-sky-700 hover:underline dark:text-sky-400">"Files & terminals →"</Go> })}
        </div>
        {machine.head.map(|h| view! { <dl class="grid min-w-0 gap-x-6 gap-y-1 text-xs sm:grid-cols-2">
            <div><dt class="text-muted">"Version / update"</dt><dd class="break-all">{machine.version.unwrap_or_else(|| "unknown".into())}" · "{update_summary(&h.update)}</dd></div>
            <div><dt class="text-muted">"Uptime / connected workers"</dt><dd>{crate::tasks::elapsed(h.uptime_secs.saturating_mul(1000).min(i64::MAX as u64) as i64)}" / "{h.connected_workers}</dd></div>
            <div class="min-w-0"><dt class="text-muted">"Data directory"</dt><dd class="break-all font-mono">{h.data_dir}</dd></div>
            <div class="min-w-0"><dt class="text-muted">"Data directory filesystem"</dt><dd class="break-all">{h.data_disk.map(|d| format!("{} ({}) · {} / {} used · {} available · {}", d.mount, d.filesystem, bytes(d.used), bytes(d.total), bytes(d.available), if d.total > 0 { format!("{:.1}% used", percent(d.used, d.total)) } else { "capacity unknown".into() })).unwrap_or_else(|| "unknown".into())}</dd></div>
            <div class="min-w-0"><dt class="text-muted">"Databases"</dt><dd class="break-all">{h.databases.into_iter().map(|d| format!("{}: {}", d.name, bytes(d.bytes))).collect::<Vec<_>>().join(" · ")}</dd></div>
        </dl> })}
        {machine.warnings.into_iter().map(|w| view! { <p class="text-amber-700 dark:text-amber-400">{w}</p> }).collect_view()}
        </div>
    }
}

#[component]
fn History(mut samples: Vec<MachineSample>, current: Option<MachineSample>, start: i64, end: i64) -> impl IntoView {
    samples.sort_by_key(|s| s.at_ms);
    let points = |f: fn(&MachineSample) -> f64| samples.iter().map(|s| (s.at_ms, f(s))).collect::<Vec<_>>();
    let oom = oom_markers(&samples);
    let (cpu_sessions, memory_sessions) = top_sessions(current.as_ref());
    let mounts: std::collections::BTreeSet<_> =
        samples.iter().flat_map(|s| s.disks.iter().filter(|d| d.total > 0).map(|d| d.mount.clone())).collect();
    view! {
        <div class="grid min-w-0 gap-3 sm:grid-cols-2 lg:grid-cols-3">
            <Chart label="CPU (%)".into() points=points(|s| s.cpu_percent) start end oom=oom.clone() />
            <Chart label="Load average (1m)".into() points=points(|s| s.load[0]) start end ceiling=1.0 oom=oom.clone() />
            <Chart label="Memory used (%)".into() points=points(|s| percent(s.memory_used, s.memory_total)) start end oom=oom.clone() />
            <Chart label="Swap used (GiB)".into() points=points(|s| s.swap_used as f64 / 1_073_741_824.0) start end ceiling=1.0 oom=oom.clone() />
            {mounts.into_iter().map(|mount| {
                let disk_points = disk_series(&samples, &mount);
                view! { <Chart label=format!("Disk {mount} (%)") points=disk_points start end oom=oom.clone() /> }
            }).collect_view()}
        </div>
        <p class="text-xs text-muted">"History: "{crate::tasks::elapsed(end - start)}" · red dashed lines mark new OOM kills. Missing samples are unknown; long sampling gaps are disconnected."</p>
        <p class="text-xs text-muted">"Longer ranges show peak envelopes: each time bucket preserves resource peaks and warnings. Different metrics in a bucket may peak at different moments; they are not a simultaneous snapshot."</p>
        {current.map(|s| view! {
            <div class="space-y-3 text-xs">
                <p class="text-muted">{format!("Current inventory sample: {}. This snapshot, partitions and session usage are independent of the history range.", crate::when::ago(now_ms() - s.at_ms))}</p>
                <p class="text-muted">{format!("{} cores · load {:.2} / {:.2} / {:.2} · memory {} used, {} available / {} · swap {} / {} · OOM kills {}", s.cpu_count, s.load[0], s.load[1], s.load[2], bytes(s.memory_used), bytes(s.memory_available), bytes(s.memory_total), bytes(s.swap_used), bytes(s.swap_total), s.oom_kills.map(|n| n.to_string()).unwrap_or_else(|| "unknown".into()))}</p>
                <h3 class="font-medium">"Current partitions"</h3>
                <div class="overflow-x-auto"><table class="w-full text-left"><thead class="text-muted"><tr><th class="py-1 pr-3">"Mount / filesystem"</th><th class="pr-3">"Used / total"</th><th class="pr-3">"Used %"</th><th class="pr-3">"Available"</th><th>"Inodes used"</th></tr></thead><tbody>{s.disks.into_iter().map(|d| view! {
                    <tr class="border-t border-zinc-200 dark:border-zinc-800"><td class="py-2 pr-3 font-mono">{d.mount}<span class="block text-muted">{d.filesystem}</span></td><td class="whitespace-nowrap pr-3">{format!("{} / {}", bytes(d.used), bytes(d.total))}</td><td class="pr-3">{if d.total > 0 { format!("{:.1}%", percent(d.used, d.total)) } else { "unknown".into() }}</td><td class="whitespace-nowrap pr-3">{bytes(d.available)}</td><td>{d.inodes_used.map(|n| n.to_string()).unwrap_or_else(|| "unknown".into())}</td></tr>
                }).collect_view()}</tbody></table></div>
                {(!s.pressure.is_empty()).then(|| view! { <details><summary>"Linux resource pressure"</summary><pre class="overflow-x-auto text-muted">{s.pressure.into_iter().map(|(k,v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\n")}</pre></details> })}
            </div>
        })}
        <div class="grid min-w-0 gap-4 sm:grid-cols-2"><SessionUsage title="Top sessions by CPU" sessions=cpu_sessions /><SessionUsage title="Top sessions by memory" sessions=memory_sessions /></div>
        <p class="text-xs text-muted">"Session CPU is relative to one logical CPU and may exceed 100%. Resident memory includes the attributed process tree."</p>
    }
}

fn top_sessions(
    current: Option<&MachineSample>,
) -> (Vec<c6_api::machines::MachineSessionUsage>, Vec<c6_api::machines::MachineSessionUsage>) {
    let mut cpu = current.map(|s| s.sessions.clone()).unwrap_or_default();
    let mut memory = cpu.clone();
    cpu.sort_by(|a, b| b.cpu_percent.total_cmp(&a.cpu_percent).then(b.rss.cmp(&a.rss)));
    memory.sort_by(|a, b| b.rss.cmp(&a.rss).then(b.cpu_percent.total_cmp(&a.cpu_percent)));
    cpu.truncate(10);
    memory.truncate(10);
    (cpu, memory)
}

#[component]
fn SessionUsage(title: &'static str, sessions: Vec<c6_api::machines::MachineSessionUsage>) -> impl IntoView {
    let live = api::live();
    view! { <div class="min-w-0 space-y-2"><h3 class="text-sm font-medium">{title}</h3>{if sessions.is_empty() { view! { <p class="text-xs text-muted">"No attributed session usage in the current inventory sample."</p> }.into_any() } else { sessions.into_iter().map(|s| {
        let title = live.session(&s.session_id).map(|s| s.title).filter(|s| !s.is_empty()).unwrap_or_else(|| s.session_id.clone());
        view! { <div class="flex min-w-0 justify-between gap-3 text-xs"><Go to=View::session(&s.session_id) class="min-w-0 truncate text-sky-700 hover:underline dark:text-sky-400">{title}</Go><span class="shrink-0">{format!("{:.1}% / {}", s.cpu_percent, bytes(s.rss))}</span></div> }
    }).collect_view().into_any() }}</div> }
}

#[component]
fn DiskScan(machine_id: String) -> impl IntoView {
    let machine_id = StoredValue::new(machine_id);
    let busy = RwSignal::new(false);
    let result = RwSignal::new(None::<MachineDiskScan>);
    let error = RwSignal::new(None::<String>);
    let alive = StoredValue::new(true);
    on_cleanup(move || alive.set_value(false));
    let start = move |_| {
        busy.set(true);
        error.set(None);
        result.set(None);
        spawn_local(async move {
            // The server runs its bounded traversal off the async runtime;
            // this explicit request returns the completed (possibly partial)
            // scan. Keep the page interactive and ignore a late unmounted reply.
            let scan = api::machine_disk_scan(machine_id.get_value()).await;
            if alive.try_get_value().unwrap_or(false) {
                match scan {
                    Ok(scan) => result.set(Some(scan)),
                    Err(e) => error.set(Some(e)),
                }
                busy.set(false);
            }
        });
    };
    view! {
        <div class="space-y-2 border-t border-zinc-200 pt-4 dark:border-zinc-800"><button class="rounded border border-zinc-300 px-3 py-1.5 text-sm disabled:opacity-50 dark:border-zinc-700" disabled=move || busy.get() || !crate::place::is_console() on:click=start>{move || if busy.get() { "Investigating disk…" } else { "What's using the disk?" }}</button>
            {(!crate::place::is_console()).then(|| view! { <p class="text-xs text-muted">"Disk investigation is available in the console."</p> })}
            <p class="text-xs text-muted">"Explicit bounded background scan. No files are deleted; inaccessible folders and scan limits may produce partial results."</p>
            {move || error.get().map(|e| view! { <crate::util::ErrorBox message=e /> })}
            {move || result.get().map(|r| view! { <div class="space-y-2 text-xs">
                <p class="text-muted">{format!("{} entries checked · {}", r.entries, if r.partial { "partial results" } else { "complete" })}</p>
                <div class="max-h-72 space-y-2 overflow-auto">{r.folders.into_iter().map(|f| view! { <div class="flex flex-wrap justify-between gap-x-4"><span class="min-w-0 break-all font-mono">{f.path}</span><span>{bytes(f.bytes)}" · "{f.modified_ms.map(|ms| format!("modified {}", crate::when::ago(now_ms() - ms))).unwrap_or_else(|| "modified unknown".into())}</span></div> }).collect_view()}</div>
            </div> })}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_freshness_respects_effective_sampling_interval_and_offline_state() {
        assert!(!sample_stale(true, Some(0), 60_000, 15));
        assert!(sample_stale(true, Some(0), 60_001, 15));
        assert!(!sample_stale(true, Some(0), 90_000, 120));
        assert!(!sample_stale(true, Some(0), 360_000, 120));
        assert!(sample_stale(true, Some(0), 360_001, 120));
        assert!(sample_stale(false, Some(90_000), 90_000, 120));
        assert!(!sample_stale(true, None, 90_000, 120));
        assert!(!sample_stale(true, Some(0), i64::MAX, u64::MAX));
        assert!(!sample_stale(true, Some(100), 0, 15));
        assert!(sample_stale(true, Some(0), 60_001, 0));
    }

    #[test]
    fn chart_empty_single_constant_and_time_domains() {
        assert_eq!(chart_path(&[], 0, 100, 100.0), "");
        assert_eq!(chart_path(&[(50, 50.0)], 0, 100, 100.0), "M150.00,40.00");
        assert_eq!(chart_path(&[(100, 0.0), (0, 0.0)], 0, 100, 0.0), "M0.00,76.00 L300.00,76.00");
        assert_eq!(chart_path(&[(0, 2.0), (50, 2.0), (100, 2.0)], 0, 100, 1.0), "M0.00,4.00 L150.00,4.00 L300.00,4.00");
        assert!(!chart_path(&[(0, 1.0)], 0, 0, 0.0).contains("NaN"));
    }

    #[test]
    fn chart_filters_unknown_values_and_out_of_range_points() {
        assert_eq!(chart_path(&[(-1, 10.0), (0, f64::NAN), (25, 10.0), (50, f64::INFINITY), (101, 0.0)], 0, 100, 100.0), "M75.00,68.80");
        assert_eq!(chart_path(&[(0, 50.0), (3_600_000, 50.0)], 0, 3_600_000, 100.0), "M0.00,40.00M300.00,40.00");
    }

    #[test]
    fn missing_capacity_and_oom_counters_are_not_fabricated_zeroes() {
        assert!(percent(0, 0).is_nan());
        assert!(fullest(&MachineSample::default()).is_nan());
        let samples: Vec<_> = [Some(4), Some(4), Some(5), None, Some(6), Some(0), Some(1)]
            .into_iter()
            .enumerate()
            .map(|(i, oom_kills)| MachineSample { at_ms: i as i64, oom_kills, ..Default::default() })
            .collect();
        assert_eq!(oom_markers(&samples), vec![2, 6]);
    }

    #[test]
    fn disk_series_does_not_invent_samples_when_a_mount_is_absent() {
        use c6_api::machines::MachineDisk;
        let samples = vec![
            MachineSample {
                at_ms: 1,
                disks: vec![MachineDisk { mount: "/data".into(), used: 25, total: 100, ..Default::default() }],
                ..Default::default()
            },
            MachineSample { at_ms: 2, ..Default::default() },
        ];
        assert_eq!(disk_series(&samples, "/data"), vec![(1, 25.0)]);
        assert!(disk_series(&samples, "/missing").is_empty());
    }

    #[test]
    fn memory_top_sessions_include_idle_memory_hogs() {
        use c6_api::machines::MachineSessionUsage;
        let mut sessions: Vec<_> =
            (0..12).map(|n| MachineSessionUsage { session_id: n.to_string(), cpu_percent: (n + 1) as f64, rss: 1 }).collect();
        sessions.push(MachineSessionUsage { session_id: "memory-hog".into(), cpu_percent: 0.0, rss: 1_000_000 });
        let (cpu, memory) = top_sessions(Some(&MachineSample { sessions, ..Default::default() }));
        assert_eq!(cpu.len(), 10);
        assert!(!cpu.iter().any(|s| s.session_id == "memory-hog"));
        assert_eq!(memory[0].session_id, "memory-hog");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn chart_markup_labels_unknown_single_points_and_oom_events() {
        let empty = view! { <Chart label="CPU (%)".to_string() points=vec![] start=0 end=100 /> }.to_html();
        assert!(empty.contains("No samples yet"));
        assert!(empty.contains("unknown"));
        let single = view! { <Chart label="CPU (%)".to_string() points=vec![(50, 25.0)] start=0 end=100 oom=vec![50] /> }.to_html();
        assert!(single.contains("<circle"));
        assert!(single.contains("New OOM kill"));
        assert!(single.contains("role=\"img\""));
        assert!(!single.contains("NaN"));
    }
}
