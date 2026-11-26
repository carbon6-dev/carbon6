//! Machine inventory and lightweight, timestamp-shaped resource charts.

use std::time::Duration;

use c6_api::machines::{Machine, MachineBuildFolder, MachineDiskScan, MachineSample};
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde_json::Value;

use crate::{
    api,
    api::Request,
    icons::{Icon, Size, icon},
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

// These console-only endpoints are intentionally not agent RPCs. Keep the
// selected realm on every request, including configuration and removal.
async fn machine_request(machine: &str, action: &str, method: &str, body: Option<serde_json::Value>) -> Result<Value, String> {
    let realm = api::realm().get_untracked();
    let query = if realm.is_empty() { String::new() } else { format!("?realm={}", api::enc(&realm)) };
    let url = format!("/api/v1/machines/{}/{}{}", api::enc(machine), action, query);
    let request = match method {
        "PUT" => Request::put(&url),
        "POST" => Request::post(&url),
        _ => Request::get(&url),
    };
    let response = if let Some(body) = body { request.json(&body).map_err(|e| e.to_string())?.send().await } else { request.send().await }
        .map_err(|e| e.to_string())?;
    if !response.ok() {
        if response.status() == 401 {
            api::signed_out().set(true);
        }
        return Err(response.json::<c6_api::ApiError>().await.map(|e| e.error).unwrap_or_else(|_| response.status_text()));
    }
    response.json().await.map_err(|e| e.to_string())
}

fn cache_hit_rate(rate: Option<f64>) -> String {
    rate.filter(|r| r.is_finite() && (0.0..=1.0).contains(r)).map(|r| format!("{:.1}%", r * 100.0)).unwrap_or_else(|| "unavailable".into())
}

// Use the existing JSON decoder and one renderer for all metric rows rather
// than adding response-specific Serde/fetch state and four view specializations.
fn cache_metrics(value: &Value) -> [(&'static str, String); 4] {
    let stats = &value["stats"];
    let count = |key: &str| stats[key].as_u64().map(|n| n.to_string()).unwrap_or_else(|| "unavailable".into());
    [
        ("Cache size", stats["cache_size_bytes"].as_u64().map(bytes).unwrap_or_else(|| "unavailable".into())),
        ("Cache hits / misses", format!("{} / {}", count("cache_hits"), count("cache_misses"))),
        ("Hit rate", cache_hit_rate(stats["hit_rate"].as_f64())),
        (
            if stats["compile_time_saved_estimated"].as_bool() == Some(true) {
                "Estimated compile time saved"
            } else {
                "Compile time saved"
            },
            cache_time_saved(stats["compile_time_saved_seconds"].as_f64()),
        ),
    ]
}

fn cache_time_saved(seconds: Option<f64>) -> String {
    seconds
        .filter(|s| s.is_finite() && *s >= 0.0)
        .map(|s| {
            if s < 60.0 {
                format!("{s:.1} s")
            } else if s < 3600.0 {
                format!("{:.1} min", s / 60.0)
            } else {
                format!("{:.1} h", s / 3600.0)
            }
        })
        .unwrap_or_else(|| "unavailable".into())
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

/// Initial history can arrive after several inventory polls. Fill its missing
/// timestamps without replacing retained polls, then upsert the latest sample.
fn merge_card_samples(samples: &mut Vec<MachineSample>, history: &[MachineSample], current: Option<&MachineSample>, end: i64) {
    for sample in history {
        if !samples.iter().any(|s| s.at_ms == sample.at_ms) {
            samples.push(sample.clone());
        }
    }
    if let Some(current) = current {
        if let Some(sample) = samples.iter_mut().find(|s| s.at_ms == current.at_ms) {
            sample.clone_from(current);
        } else {
            samples.push(current.clone());
        }
    }
    samples.retain(|s| s.at_ms >= end.saturating_sub(3_600_000) && s.at_ms <= end);
    samples.sort_by_key(|s| s.at_ms);
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
            <div class="mt-1 flex justify-between text-[10px] text-muted"><span>{format!("0–{domain:.0}")}</span><span>"latest "{icon(Icon::ArrowRight, Size::S14)}</span></div>
        </figure>
    }
}

#[component]
pub fn Machines() -> impl IntoView {
    let machines = LocalResource::new(move || {
        api::realm().get();
        api::machines()
    });
    let inventory = RwSignal::new(Vec::<Machine>::new());
    Effect::new(move || {
        if let Some(Ok(mut ms)) = machines.get() {
            ms.sort_by_key(|m| (m.role != "head", m.name.clone()));
            inventory.set(ms);
        }
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
            <Transition fallback=|| view! { <p class="text-sm text-muted">"Loading machines…"</p> }>
                {move || machines.get().and_then(Result::err).map(|e| view! { <crate::util::ErrorBox message=e /> })}
            </Transition>
            <div class="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
                <For each=move || inventory.get() key=|m| m.id.clone() children=move |initial| {
                    let machine = Signal::derive(move || inventory.with(|ms| ms.iter().find(|m| m.id == initial.id).cloned()).unwrap_or_else(|| initial.clone()));
                    view! { <MachineCard machine selected /> }
                } />
            </div>
            {move || view! { <MachineDetails machine_id=selected.get() /> }}
        </section>
    }
}

#[component]
fn MachineCard(machine: Signal<Machine>, selected: RwSignal<String>) -> impl IntoView {
    let id = StoredValue::new(machine.with_untracked(|m| m.id.clone()));
    let stale = move || machine.with(|m| sample_stale(m.online, m.last_sample.as_ref().map(|s| s.at_ms), now_ms(), m.sample_secs));
    let sampled = move || {
        machine.with(|m| {
            m.last_sample
                .as_ref()
                .map(|s| format!("Sample {}", crate::when::ago(now_ms() - s.at_ms)))
                .unwrap_or_else(|| "No telemetry received".into())
        })
    };
    let until = now_ms();
    let history = LocalResource::new(move || api::machine_samples(id.get_value(), until - 3_600_000, until));
    let samples = RwSignal::new(Vec::<MachineSample>::new());
    let end = RwSignal::new(until);
    Effect::new(move || {
        let history = history.get().and_then(Result::ok).unwrap_or_default();
        let current = machine.with(|m| m.last_sample.clone());
        let until = now_ms().max(current.as_ref().map_or(i64::MIN, |s| s.at_ms));
        samples.update(|ss| merge_card_samples(ss, &history, current.as_ref(), until));
        end.set(until);
    });
    view! {
        <button class="min-w-0 space-y-3 rounded-lg border border-zinc-200 bg-white p-4 text-left hover:border-sky-500 aria-pressed:border-sky-500 aria-pressed:bg-sky-50 dark:border-zinc-800 dark:bg-zinc-900 dark:hover:border-sky-500 dark:aria-pressed:bg-sky-950"
            data-machine-card=id.get_value()
            aria-pressed=move || selected.get() == id.get_value()
            on:click=move |_| selected.set(id.get_value())>
            <div class="flex min-w-0 items-center gap-2"><span class=move || if machine.with(|m| m.online) { "h-2 w-2 shrink-0 rounded-full bg-emerald-500" } else { "h-2 w-2 shrink-0 rounded-full bg-zinc-400" }></span><span class="min-w-0 break-words font-medium">{move || machine.with(|m| m.name.clone())}</span><span class="ml-auto text-xs text-muted">{move || machine.with(|m| m.role.clone())}</span></div>
            <p class="text-xs text-muted">{move || if machine.with(|m| m.online) { "online" } else { "offline" }}" · "{move || machine.with(|m| m.version.clone().unwrap_or_else(|| "version unknown".into()))}</p>
            <p class="break-all text-xs text-muted">{move || machine.with(|m| m.hostname.clone())}</p>
            {move || machine.with(|m| (!m.warnings.is_empty()).then(|| view! { <span class="inline-flex items-center gap-1 rounded-full bg-amber-100 px-2 py-0.5 text-xs font-medium text-amber-900 dark:bg-amber-950 dark:text-amber-300" role="status">{icon(Icon::TriangleAlert, Size::S14)}{format!("{} warning{}", m.warnings.len(), if m.warnings.len() == 1 { "" } else { "s" })}</span> }))}
            <div class="space-y-2">{["CPU", "Memory", "Fullest disk"].into_iter().map(|label| view! {
                <div><div class="flex justify-between gap-2 text-xs"><span class="shrink-0 text-muted">{label}</span><span class="min-w-0 break-all text-right">{move || machine.with(|m| m.last_sample.as_ref().map(|s| match label {
                    "CPU" => format!("{:.0}% · {} cores", s.cpu_percent, s.cpu_count),
                    "Memory" => format!("{} / {}", bytes(s.memory_used), bytes(s.memory_total)),
                    _ => s.disks.iter().filter(|d| d.total > 0)
                        .max_by(|a, b| percent(a.used, a.total).total_cmp(&percent(b.used, b.total)))
                        .map(|d| format!("{:.0}% · {}", percent(d.used, d.total), d.mount))
                        .unwrap_or_else(|| "unknown".into()),
                }).unwrap_or_else(|| "unknown".into()))}</span></div><svg viewBox="0 0 300 80" class="mt-1 h-6 w-full text-sky-600 dark:text-sky-400" role="img" aria-label=format!("{label} over the last hour") preserveAspectRatio="none"><path d=move || {
                    let points: Vec<_> = samples.with(|ss| ss.iter().map(|s| (s.at_ms, match label { "CPU" => s.cpu_percent, "Memory" => percent(s.memory_used, s.memory_total), _ => fullest(s) })).collect());
                    chart_path(&points, end.get() - 3_600_000, end.get(), 100.0)
                } fill="none" stroke="currentColor" stroke-width="1.5" vector-effect="non-scaling-stroke" /></svg></div>
            }).collect_view()}</div>
            <p class=move || if stale() { "text-xs text-amber-700 dark:text-amber-400" } else { "text-xs text-muted" }>{sampled}{move || stale().then_some(" · stale / offline")}</p>
            {move || machine.with(|m| m.warnings.iter().map(|w| view! { <p class="text-xs text-amber-700 dark:text-amber-400">{w.clone()}</p> }).collect_view())}
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
            <Transition fallback=|| ()>{move || inventory.get().map(|r| match r {
                Ok(ms) => ms.into_iter().find(|m| m.id == machine_id.get_value()).map(|m| view! { <MachineInfo machine=m /> }).into_any(),
                Err(e) => view! { <crate::util::ErrorBox message=e /> }.into_any(),
            })}</Transition>
            <Transition fallback=|| view! { <p class="text-sm text-muted">"Loading history…"</p> }>{move || samples.get().map(|r| match r {
                Ok(ss) => {
                    let current = inventory.get().and_then(Result::ok).and_then(|ms| ms.into_iter().find(|m| m.id == machine_id.get_value())).and_then(|m| m.last_sample);
                    view! { <History samples=ss current start=end.get() - range.get() end=end.get() /> }.into_any()
                },
                Err(e) => view! { <crate::util::ErrorBox message=e /> }.into_any(),
            })}</Transition>
            {(machine_id.get_value() != "head" && crate::place::is_console()).then(move || view! { <CompileCache machine_id=machine_id.get_value() /> })}
            <DiskScan machine_id=machine_id.get_value() />
        </section>
    }
}

#[component]
fn MachineInfo(machine: Machine) -> impl IntoView {
    let head = machine.role == "head";
    view! {
        <div class="space-y-2 text-sm"><div class="flex flex-wrap items-center gap-2"><strong class="break-all">{machine.name}</strong><span class="text-muted">{machine.role}" · "{if machine.online { "online" } else { "offline" }}</span>
            {(!head).then(|| view! { <Go to=View::Worker { id: machine.id.clone() } class="text-sky-700 hover:underline dark:text-sky-400">"Files & terminals "{icon(Icon::ArrowRight, Size::S14)}</Go> })}
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
fn CompileCache(machine_id: String) -> impl IntoView {
    let machine_id = StoredValue::new(machine_id);
    let status = LocalResource::new(move || {
        api::realm().get();
        async move {
            let value = machine_request(&machine_id.get_value(), "compile_cache", "GET", None).await?;
            if !value["config"].is_object() {
                return Err("Worker returned an invalid cache configuration".into());
            }
            Ok::<Value, String>(value)
        }
    });
    // Poll telemetry without replacing the editor or losing unsaved changes.
    let initial = RwSignal::new(None::<Value>);
    Effect::new(move || {
        if initial.with_untracked(Option::is_none)
            && let Some(Ok(value)) = status.get()
        {
            initial.set(Some(value["config"].clone()));
        }
    });
    let poll = set_interval_with_handle(move || status.refetch(), Duration::from_secs(60)).ok();
    on_cleanup(move || {
        if let Some(poll) = poll {
            poll.clear();
        }
    });
    view! {
        <div class="space-y-3 border-t border-zinc-200 pt-4 dark:border-zinc-800">
            <div class="flex flex-wrap items-center justify-between gap-2"><h3 class="text-sm font-semibold">"Shared compile cache"</h3>
                <button class="text-xs text-sky-700 hover:underline dark:text-sky-400" on:click=move |_| status.refetch()>"Refresh cache status"</button>
            </div>
            <p class="text-xs text-muted">"Opt-in per worker. Shares compiler outputs, not Cargo target directories. Existing compiler selections are not overwritten. Changes apply on the next turn; Claude Code takes them up at its next safe idle restart, never interrupting running tasks. Status refreshes every minute."</p>
            <Transition fallback=|| view! { <p class="text-xs text-muted">"Loading compile cache…"</p> }>
                {move || status.get().map(|r| match r {
                    Ok(value) => view! { <CacheTelemetry value /> }.into_any(),
                    Err(e) => view! { <crate::util::ErrorBox message=e /> }.into_any(),
                })}
            </Transition>
            <Show when=move || initial.with(Option::is_some)>
                <CacheSettings machine_id=machine_id.get_value() config=initial.get_untracked().expect("initial cache configuration") refresh=Callback::new(move |()| status.refetch()) />
            </Show>
        </div>
    }
}

#[component]
fn CacheTelemetry(value: Value) -> impl IntoView {
    let directory = value["directory"].as_str().unwrap_or("unavailable").to_owned();
    let status = value["status"].as_str().unwrap_or("unavailable").to_owned();
    let cap = value["max_bytes"].as_u64().map(bytes).unwrap_or_else(|| "unavailable".into());
    let install = (status == "missing").then(|| value["install_command"].as_str().unwrap_or("cargo install sccache --locked").to_owned());
    let error = value["stats_error"].as_str().map(str::to_owned);
    view! {
        <div class="space-y-3 text-xs">
            <p role="status" class="break-all">{format!("{status} · {directory} · cap {cap}")}</p>
            {install.map(|command| view! { <div class="space-y-1 rounded bg-amber-50 p-3 dark:bg-amber-950">
                <p>"sccache is missing on this worker. Run this there, then refresh (nothing is installed automatically):"</p>
                <code class="block break-all select-all">{command}</code>
            </div> })}
            <dl class="grid gap-2 sm:grid-cols-2 lg:grid-cols-4">
                {cache_metrics(&value).into_iter().map(|(label, text)| view! { <div><dt class="text-muted">{label}</dt><dd>{text}</dd></div> }).collect_view()}
            </dl>
            {error.map(|e| view! { <p class="text-muted">"Statistics unavailable: "{e}</p> })}
        </div>
    }
}

#[component]
fn CacheSettings(machine_id: String, config: Value, refresh: Callback<()>) -> impl IntoView {
    let machine_id = StoredValue::new(machine_id);
    let enabled = RwSignal::new(config["enabled"].as_bool().unwrap_or(false));
    let directory = RwSignal::new(config["directory"].as_str().unwrap_or_default().to_owned());
    let cap = RwSignal::new(config["max_bytes"].as_u64().map(|n| n.to_string()).unwrap_or_default());
    let rust = RwSignal::new(config["rust"].as_bool().unwrap_or(true));
    let c_cpp = RwSignal::new(config["c_cpp"].as_bool().unwrap_or(false));
    let grace = RwSignal::new(config["archived_grace_days"].as_u64().unwrap_or(3).to_string());
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let alive = StoredValue::new(true);
    on_cleanup(move || alive.set_value(false));
    let save = move |_| {
        if busy.get_untracked() || !crate::place::is_console() {
            return;
        }
        let max_bytes = match cap.get_untracked().trim() {
            "" => None,
            n => match n.parse::<u64>() {
                Ok(n) if n > 0 => Some(n),
                _ => {
                    error.set(Some("Size cap must be a positive number of bytes, or blank for the worker default.".into()));
                    return;
                }
            },
        };
        let archived_grace_days = match grace.get_untracked().trim().parse::<u32>() {
            Ok(n) => n,
            Err(_) => {
                error.set(Some("Archive grace period must be a whole number of days.".into()));
                return;
            }
        };
        let path = directory.get_untracked().trim().to_owned();
        let config = serde_json::json!({
            "enabled": enabled.get_untracked(), "directory": (!path.is_empty()).then_some(path),
            "max_bytes": max_bytes, "rust": rust.get_untracked(), "c_cpp": c_cpp.get_untracked(),
            "archived_grace_days": archived_grace_days,
        });
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let saved = machine_request(&machine_id.get_value(), "compile_cache", "PUT", Some(config)).await;
            if alive.try_get_value().unwrap_or(false) {
                busy.set(false);
                match saved {
                    Ok(_) => refresh.run(()),
                    Err(e) => error.set(Some(e)),
                }
            }
        });
    };
    view! {
        <div class="space-y-3 text-xs">
            <fieldset class="space-y-3" disabled=move || busy.get() || !crate::place::is_console()>
                <div class="flex flex-wrap gap-4">
                    {[("Enable shared compile cache", enabled), ("Rust", rust), ("C / C++", c_cpp)].into_iter().map(|(label, flag)| view! {
                        <label class="flex items-center gap-2"><input type="checkbox" class=crate::components::CHECKBOX prop:checked=move || flag.get() on:change=move |e| flag.set(event_target_checked(&e)) />{label}</label>
                    }).collect_view()}
                </div>
                <div class="grid gap-3 sm:grid-cols-2">
                    {[("Cache directory (blank: worker default)", directory, "text"), ("Size cap in bytes (blank: worker default)", cap, "numeric"), ("Archived build-folder grace period (days)", grace, "numeric")].into_iter().map(|(label, text, mode)| view! {
                        <label class="space-y-1">{label}<input class="w-full rounded border border-zinc-300 bg-transparent p-2 dark:border-zinc-700" type="text" inputmode=mode prop:value=move || text.get() on:input=move |e| text.set(event_target_value(&e)) /></label>
                    }).collect_view()}
                </div>
                <button class="rounded border border-zinc-300 px-3 py-1.5 disabled:opacity-50 dark:border-zinc-700" on:click=save>{move || if busy.get() { "Saving…" } else { "Save cache settings" }}</button>
            </fieldset>
            {(!crate::place::is_console()).then(|| view! { <p class="text-muted">"Cache configuration is available in the console."</p> })}
            {move || error.get().map(|e| view! { <crate::util::ErrorBox message=e /> })}
        </div>
    }
}

#[component]
fn BuildFolderRow(machine_id: String, folder: MachineBuildFolder, removed: Callback<String>) -> impl IntoView {
    let machine_id = StoredValue::new(machine_id);
    let path = StoredValue::new(folder.path.clone());
    let incremental = folder.incremental;
    let protected = folder.active || folder.partial;
    let busy = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let alive = StoredValue::new(true);
    on_cleanup(move || alive.set_value(false));
    let remove = move |_| {
        if busy.get_untracked() || protected || !crate::place::is_console() {
            return;
        }
        let target = path.get_value();
        let message = format!(
            "Remove {}\n\n{}\n\nThis permanently deletes all files in the selected build-output folder and cannot be undone. The next build must rebuild its outputs. The worker rechecks that no session or terminal is using this folder. Continue?",
            if incremental { "this incremental build folder?" } else { "this build folder and all its outputs?" },
            target
        );
        if !window().confirm_with_message(&message).unwrap_or(false) {
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            let response = machine_request(
                &machine_id.get_value(),
                "build_folders/remove",
                "POST",
                Some(serde_json::json!({
                    "path": target, "incremental_only": incremental, "confirmed": true,
                })),
            )
            .await;
            if alive.try_get_value().unwrap_or(false) {
                busy.set(false);
                match response {
                    Ok(value) => {
                        let errors: Vec<_> = value
                            .get("errors")
                            .and_then(serde_json::Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(serde_json::Value::as_str)
                            .collect();
                        let count = value.get("removed").and_then(serde_json::Value::as_array).map_or(0, Vec::len);
                        if errors.is_empty() && count > 0 {
                            removed.run(format!(
                                "Removed {count} build folder(s). Investigate again to refresh folder sizes and attribution."
                            ));
                        } else if errors.is_empty() {
                            error.set(Some("No build folders were removed. Investigate again; the folder may now be active or the new scan incomplete.".into()));
                        } else {
                            error.set(Some(errors.join("; ")));
                        }
                    }
                    Err(e) => error.set(Some(e)),
                }
            }
        });
    };
    let sessions = folder.session_ids.clone();
    view! {
        <div class="space-y-2 rounded border border-zinc-200 p-3 dark:border-zinc-800">
            <div class="flex flex-wrap justify-between gap-2"><span class="min-w-0 break-all font-mono">{folder.path}</span><span>{bytes(folder.bytes)}</span></div>
            <p class="text-muted">{format!("{} · {}{}{}", if folder.carbon6_owned { "carbon6-owned" } else { "external — never automatically cleaned" },
                if folder.active { "active — protected" } else if folder.archived_at_ms.is_some() { "archived" } else { "inactive" },
                if folder.incremental { " · incremental" } else { "" }, if folder.partial { " · partial scan — protected" } else { "" })}</p>
            <p class="text-muted">{folder.last_used_ms.map(|ms| format!("Last used {}", crate::when::ago(now_ms() - ms))).unwrap_or_else(|| "Last use unknown".into())}
                " · "{folder.modified_ms.map(|ms| format!("modified {}", crate::when::ago(now_ms() - ms))).unwrap_or_else(|| "modified unknown".into())}
                {folder.archived_at_ms.map(|ms| format!(" · archived {}", crate::when::ago(now_ms() - ms)))}
            </p>
            {(!folder.repositories.is_empty()).then(|| view! { <p class="break-all text-muted">"Repositories: "{folder.repositories.join(", ")}</p> })}
            {(!sessions.is_empty()).then(|| view! { <div class="flex flex-wrap gap-2"><span class="text-muted">"Sessions:"</span>{sessions.into_iter().map(|id| view! {
                <Go to=View::session(&id) class="break-all text-sky-700 hover:underline dark:text-sky-400">{id}</Go>
            }).collect_view()}</div> })}
            <button class="rounded border border-red-300 px-3 py-1.5 text-red-700 disabled:opacity-50 dark:border-red-900 dark:text-red-400"
                disabled=move || protected || busy.get() || !crate::place::is_console() on:click=remove>
                {move || if busy.get() { "Removing…" } else if incremental { "Remove incremental outputs…" } else { "Remove build folder…" }}
            </button>
            {move || error.get().map(|e| view! { <crate::util::ErrorBox message=e /> })}
        </div>
    }
}

#[component]
fn DiskScan(machine_id: String) -> impl IntoView {
    let machine_id = StoredValue::new(machine_id);
    let busy = RwSignal::new(false);
    let result = RwSignal::new(None::<MachineDiskScan>);
    let error = RwSignal::new(None::<String>);
    let notice = RwSignal::new(None::<String>);
    let alive = StoredValue::new(true);
    on_cleanup(move || alive.set_value(false));
    let start = move |_| {
        busy.set(true);
        error.set(None);
        result.set(None);
        notice.set(None);
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
            <p class="text-xs text-muted">"Investigation never deletes files. Inaccessible folders and scan limits may produce partial results. Recognized inactive build folders can be removed separately with confirmation; active or incompletely scanned folders are protected."</p>
            {move || error.get().map(|e| view! { <crate::util::ErrorBox message=e /> })}
            {move || notice.get().map(|n| view! { <p role="status" class="text-xs text-muted">{n}</p> })}
            {move || result.get().map(|r| view! { <div class="space-y-2 text-xs">
                <p class="text-muted">{format!("{} entries checked · {}", r.entries, if r.partial { "partial results" } else { "complete" })}</p>
                <div class="max-h-72 space-y-2 overflow-auto">{r.folders.into_iter().map(|f| view! { <div class="flex flex-wrap justify-between gap-x-4"><span class="min-w-0 break-all font-mono">{f.path}</span><span>{bytes(f.bytes)}" · "{f.modified_ms.map(|ms| format!("modified {}", crate::when::ago(now_ms() - ms))).unwrap_or_else(|| "modified unknown".into())}</span></div> }).collect_view()}</div>
                {(machine_id.get_value() != "head").then(|| view! {
                    <h3 class="pt-3 text-sm font-semibold">"Build folders"</h3>
                    <p class="text-muted">"Sizes include descendants; target and incremental rows may overlap. Removal is permanent and requires a rebuild. External folders are never automatically cleaned."</p>
                    {(r.build_folders.is_empty()).then(|| view! { <p class="text-muted">"No recognized build folders in this scan."</p> })}
                    <div class="max-h-96 space-y-3 overflow-auto">{r.build_folders.into_iter().map(|folder| view! {
                        <BuildFolderRow machine_id=machine_id.get_value() folder removed=Callback::new(move |message| { result.set(None); notice.set(Some(message)); }) />
                    }).collect_view()}</div>
                })}
            </div> })}
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_status() -> serde_json::Value {
        serde_json::json!({
            "config": {"enabled": false, "directory": null, "max_bytes": null, "rust": true, "c_cpp": false, "archived_grace_days": 3},
            "status": "disabled", "install_command": "cargo install sccache --locked",
            "directory": "/worker/sccache", "max_bytes": 1024, "stats": null,
        })
    }

    #[test]
    fn cache_status_accepts_unavailable_and_partial_statistics() {
        let mut value = cache_status();
        let metrics = cache_metrics(&value);
        assert_eq!(metrics[0].1, "unavailable");
        assert_eq!(metrics[1].1, "unavailable / unavailable");
        assert_eq!(metrics[2].1, "unavailable");
        assert_eq!(metrics[3].1, "unavailable");
        value["stats"] = serde_json::json!({"cache_hits": 12, "compile_time_saved_seconds": 120.0, "compile_time_saved_estimated": true});
        let metrics = cache_metrics(&value);
        assert_eq!(metrics[1].1, "12 / unavailable");
        assert_eq!(metrics[3], ("Estimated compile time saved", "2.0 min".into()));
    }

    #[test]
    fn cache_metrics_never_invent_missing_savings_or_hit_rate() {
        assert_eq!(cache_hit_rate(None), "unavailable");
        assert_eq!(cache_hit_rate(Some(f64::NAN)), "unavailable");
        assert_eq!(cache_hit_rate(Some(2.0)), "unavailable");
        assert_eq!(cache_hit_rate(Some(0.25)), "25.0%");
        assert_eq!(cache_time_saved(None), "unavailable");
        assert_eq!(cache_time_saved(Some(-1.0)), "unavailable");
        assert_eq!(cache_time_saved(Some(f64::INFINITY)), "unavailable");
        assert_eq!(cache_time_saved(Some(1.5)), "1.5 s");
        assert_eq!(cache_time_saved(Some(150.0)), "2.5 min");
        assert_eq!(cache_time_saved(Some(7200.0)), "2.0 h");
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn cache_markup_shows_missing_install_command_and_labels_estimates() {
        let mut value = cache_status();
        value["status"] = serde_json::json!("missing");
        let html = view! { <CacheTelemetry value=value.clone() /> }.to_html();
        assert!(html.contains("cargo install sccache --locked"));
        assert!(html.contains("nothing is installed automatically"));
        assert!(html.contains("unavailable"));
        value["stats"] = serde_json::json!({"compile_time_saved_seconds": 120.0, "compile_time_saved_estimated": true});
        let html = view! { <CacheTelemetry value /> }.to_html();
        assert!(html.contains("Estimated compile time saved"));
        assert!(html.contains("2.0 min"));
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn cache_cleanup_markup_protects_active_and_partial_folders() {
        Owner::new().with(|| {
            for (active, partial, label) in [(true, false, "active — protected"), (false, true, "partial scan — protected")] {
                let folder = MachineBuildFolder { path: "/work/repo/target".into(), active, partial, ..Default::default() };
                let html =
                    view! { <BuildFolderRow machine_id="worker".to_owned() folder removed=Callback::new(|_: String| ()) /> }.to_html();
                assert!(html.contains(label));
                assert!(html.contains("disabled"));
                assert!(html.contains("external — never automatically cleaned"));
                assert!(html.contains("Remove build folder…"));
            }
            let folder = MachineBuildFolder { path: "/work/repo/target/debug/incremental".into(), incremental: true, ..Default::default() };
            let html = view! { <BuildFolderRow machine_id="worker".to_owned() folder removed=Callback::new(|_: String| ()) /> }.to_html();
            assert!(html.contains("Remove incremental outputs…"));
        });
    }

    #[test]
    fn card_history_merges_late_initial_history_without_losing_polls() {
        let sample = |at_ms, cpu_percent| MachineSample { at_ms, cpu_percent, ..Default::default() };
        let mut samples = Vec::new();
        merge_card_samples(&mut samples, &[], Some(&sample(20, 20.0)), 20);
        merge_card_samples(&mut samples, &[], Some(&sample(30, 30.0)), 30);
        merge_card_samples(&mut samples, &[sample(20, 1.0), sample(10, 10.0)], Some(&sample(40, 40.0)), 40);
        assert_eq!(samples.iter().map(|s| (s.at_ms, s.cpu_percent)).collect::<Vec<_>>(), [(10, 10.0), (20, 20.0), (30, 30.0), (40, 40.0)]);
    }

    #[test]
    fn card_history_upserts_timestamps_and_preserves_samples_without_current_telemetry() {
        let old = MachineSample { at_ms: 100, cpu_percent: 10.0, ..Default::default() };
        let current = MachineSample { cpu_percent: 75.0, ..old.clone() };
        let mut samples = vec![old.clone()];
        merge_card_samples(&mut samples, std::slice::from_ref(&old), Some(&current), 100);
        merge_card_samples(&mut samples, &[old], None, 200);
        assert_eq!(samples, vec![current]);
    }

    #[test]
    fn card_history_prunes_the_rolling_hour_including_late_history() {
        let sample = |at_ms| MachineSample { at_ms, ..Default::default() };
        let history = [sample(0), sample(1), sample(3_600_002)];
        let mut samples = vec![sample(-1), sample(2)];
        merge_card_samples(&mut samples, &history, Some(&sample(3_600_001)), 3_600_001);
        assert_eq!(samples.iter().map(|s| s.at_ms).collect::<Vec<_>>(), [1, 2, 3_600_001]);
        merge_card_samples(&mut samples, &history, None, 3_600_003);
        assert_eq!(samples.iter().map(|s| s.at_ms).collect::<Vec<_>>(), [3_600_001, 3_600_002]);
        merge_card_samples(&mut samples, &[], None, i64::MAX);
        assert!(samples.is_empty());
        merge_card_samples(&mut samples, &[], Some(&sample(i64::MIN)), i64::MIN);
        assert_eq!(samples[0].at_ms, i64::MIN);
    }

    #[test]
    fn card_history_prunes_unchanged_inventory_samples_as_time_advances() {
        let current = MachineSample { at_ms: 0, ..Default::default() };
        let mut samples = Vec::new();
        merge_card_samples(&mut samples, &[], Some(&current), 0);
        merge_card_samples(&mut samples, &[], Some(&current), 3_600_000);
        assert_eq!(samples, vec![current.clone()]);
        merge_card_samples(&mut samples, &[], Some(&current), 3_600_001);
        assert!(samples.is_empty());
    }

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
