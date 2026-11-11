//! Settings → Backups (docs/backups.md): the key backups are encrypted to,
//! where they go, and how they stand.

use c6_api::{BackupDestination, BackupSettings, BackupStatus};
use leptos::prelude::*;
use leptos::task::spawn_local;

use crate::util::{BUTTON, BUTTON_QUIET, ErrorBox, INPUT};

fn size(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.0} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

fn new_destination(kind: &str, n: usize) -> BackupDestination {
    BackupDestination {
        name: if kind == "s3" { format!("s3-{n}") } else { format!("nas-{n}") },
        kind: kind.into(),
        path: String::new(),
        endpoint: String::new(),
        bucket: String::new(),
        region: String::new(),
        prefix: "carbon6/".into(),
        access_key_secret: String::new(),
        secret_key_secret: String::new(),
        keep: 4,
        enabled: true,
    }
}

#[component]
pub fn BackupsSetting() -> impl IntoView {
    // Loaded once into a signal (edits stay put while status refreshes).
    let settings = RwSignal::new(None::<BackupSettings>);
    let status = RwSignal::new(None::<BackupStatus>);
    let error = RwSignal::new(None::<String>);
    let saved = RwSignal::new(false);
    let busy = RwSignal::new(false);
    let private = RwSignal::new(None::<String>);
    let restore_key = RwSignal::new(String::new());
    spawn_local(async move {
        match crate::api::backup_settings().await {
            Ok(s) => settings.set(Some(s)),
            Err(e) => error.set(Some(e)),
        }
        if let Ok(s) = crate::api::backups().await {
            status.set(Some(s));
        }
    });
    let refresh = move || {
        spawn_local(async move {
            if let Ok(s) = crate::api::backups().await {
                status.set(Some(s));
            }
        })
    };
    let edit = move |f: Box<dyn FnOnce(&mut BackupSettings)>| {
        settings.update(|s| {
            if let Some(s) = s {
                f(s);
            }
        });
        saved.set(false);
    };
    let save = move |_| {
        let Some(s) = settings.get_untracked() else { return };
        spawn_local(async move {
            match crate::api::put_backup_settings(s).await {
                Ok(()) => {
                    error.set(None);
                    saved.set(true);
                }
                Err(e) => error.set(Some(e)),
            }
            refresh();
        })
    };
    let make_key = move |_| {
        spawn_local(async move {
            match crate::api::new_backup_key().await {
                Ok(k) => {
                    private.set(Some(k.private));
                    settings.update(|s| {
                        if let Some(s) = s {
                            s.recipient = k.public;
                        }
                    });
                }
                Err(e) => error.set(Some(e)),
            }
            refresh();
        })
    };
    let act = move |what: &'static str| {
        busy.set(true);
        spawn_local(async move {
            let r = match what {
                "test" => crate::api::test_backups().await,
                "base" => crate::api::run_backups(true).await,
                _ => crate::api::run_backups(false).await,
            };
            match r {
                Ok(s) => status.set(Some(s)),
                Err(e) => error.set(Some(e)),
            }
            busy.set(false);
        })
    };
    let keep_restore_key = move |_| {
        let k = restore_key.get_untracked();
        spawn_local(async move {
            match crate::api::put_backup_restore_key(k).await {
                Ok(()) => {
                    restore_key.set(String::new());
                    error.set(None);
                }
                Err(e) => error.set(Some(e)),
            }
            refresh();
        })
    };
    let number = |v: String| v.trim().parse::<u64>().ok();
    view! {
        <section class="space-y-3 rounded-lg border border-zinc-200 p-4 dark:border-zinc-800">
            <h2 class="font-medium">"Backups"</h2>
            <p class="text-sm text-muted">
                "Everything the head keeps, sent to a folder (an NFS mount, a disk) and to S3 as it changes, encrypted to your public key. Each database's changes go within the interval; a base copy each week sends only what changed. Restore any point in time with c6h restore (docs/backups.md)."
            </p>
            {move || error.get().map(|e| view! { <ErrorBox message=e /> })}
            {move || private.get().map(|k| view! {
                <div class="space-y-1 rounded-md border border-amber-300 bg-amber-50 p-3 text-sm dark:border-amber-800 dark:bg-amber-950">
                    <p class="font-medium">"Your backup key: keep it somewhere safe, away from this machine. It is shown only now, and nothing restores without it."</p>
                    <pre class="select-all overflow-auto font-mono text-xs">{k}</pre>
                    <button class=BUTTON_QUIET on:click=move |_| private.set(None)>"I've kept it"</button>
                </div>
            })}
            {move || settings.get().map(|s| {
                let recipient = s.recipient.clone();
                view! {
                    <div class="space-y-3">
                        <div class="flex flex-wrap items-center gap-2 text-sm">
                            <span class="text-muted">"Encrypted to"</span>
                            {if recipient.is_empty() {
                                view! { <span>"no key yet (backups are off)"</span> }.into_any()
                            } else {
                                view! { <code class="truncate text-xs" title=recipient.clone()>{recipient.chars().take(24).collect::<String>()}"…"</code> }.into_any()
                            }}
                            <button class=BUTTON_QUIET on:click=make_key
                                title="Makes a new key pair; backups after this are encrypted to it (older ones still need the old key)">
                                {if recipient.is_empty() { "Make a key" } else { "Make a new key" }}
                            </button>
                        </div>
                        <div class="space-y-2">
                            {s.destinations.iter().cloned().enumerate().map(|(i, d)| {
                                let s3 = d.kind == "s3";
                                let field = move |label: &'static str, value: String, set: fn(&mut BackupDestination, String)| view! {
                                    <label class="flex min-w-0 flex-1 flex-col text-xs text-muted">
                                        {label}
                                        <input class=INPUT prop:value=value
                                            on:change=move |ev| { let v = event_target_value(&ev); edit(Box::new(move |s| if let Some(d) = s.destinations.get_mut(i) { set(d, v) })) } />
                                    </label>
                                };
                                view! {
                                    <div class="space-y-2 rounded-md border border-zinc-200 p-3 dark:border-zinc-800">
                                        <div class="flex flex-wrap items-end gap-2">
                                            {field("Name", d.name.clone(), |d, v| d.name = v)}
                                            <span class="pb-2 text-xs text-muted">{if s3 { "S3" } else { "Folder" }}</span>
                                            <label class="flex flex-col text-xs text-muted">
                                                "Keep base copies"
                                                <input type="number" min="1" class=format!("{INPUT} w-20!") prop:value=d.keep.to_string()
                                                    on:change=move |ev| if let Some(n) = number(event_target_value(&ev)) { edit(Box::new(move |s| if let Some(d) = s.destinations.get_mut(i) { d.keep = n as u32 })) } />
                                            </label>
                                            <label class="flex min-h-8 items-center gap-1 pb-2 text-sm pointer-coarse:min-h-11">
                                                <input type="checkbox" class=crate::components::CHECKBOX prop:checked=d.enabled
                                                    on:change=move |ev| { let on = event_target_checked(&ev); edit(Box::new(move |s| if let Some(d) = s.destinations.get_mut(i) { d.enabled = on })) } />
                                                "On"
                                            </label>
                                            <button class=BUTTON_QUIET on:click=move |_| edit(Box::new(move |s| { s.destinations.remove(i); }))>"Remove"</button>
                                        </div>
                                        {if s3 {
                                            view! {
                                                <div class="flex flex-wrap gap-2">
                                                    {field("Endpoint", d.endpoint.clone(), |d, v| d.endpoint = v)}
                                                    {field("Bucket", d.bucket.clone(), |d, v| d.bucket = v)}
                                                    {field("Region", d.region.clone(), |d, v| d.region = v)}
                                                    {field("Prefix", d.prefix.clone(), |d, v| d.prefix = v)}
                                                    {field("Access key (secret's name)", d.access_key_secret.clone(), |d, v| d.access_key_secret = v)}
                                                    {field("Secret key (secret's name)", d.secret_key_secret.clone(), |d, v| d.secret_key_secret = v)}
                                                </div>
                                            }.into_any()
                                        } else {
                                            view! { <div class="flex">{field("Folder (full path)", d.path.clone(), |d, v| d.path = v)}</div> }.into_any()
                                        }}
                                    </div>
                                }
                            }).collect_view()}
                            <div class="flex gap-2">
                                <button class=BUTTON_QUIET on:click=move |_| edit(Box::new(|s| { let n = s.destinations.len() + 1; s.destinations.push(new_destination("folder", n)) }))>{crate::icons::icon(crate::icons::Icon::Plus, crate::icons::Size::S14)}" Folder"</button>
                                <button class=BUTTON_QUIET on:click=move |_| edit(Box::new(|s| { let n = s.destinations.len() + 1; s.destinations.push(new_destination("s3", n)) }))>{crate::icons::icon(crate::icons::Icon::Plus, crate::icons::Size::S14)}" S3"</button>
                            </div>
                        </div>
                        <div class="flex flex-wrap gap-3 text-sm">
                            <label class="flex items-center gap-1">"Send changes every"
                                <input type="number" min="5" class=format!("{INPUT} w-20!") prop:value=s.interval_secs.to_string()
                                    on:change=move |ev| if let Some(n) = number(event_target_value(&ev)) { edit(Box::new(move |s| s.interval_secs = n)) } />"s"</label>
                            <label class="flex items-center gap-1">"New base copy every"
                                <input type="number" min="1" class=format!("{INPUT} w-16!") prop:value=s.generation_days.to_string()
                                    on:change=move |ev| if let Some(n) = number(event_target_value(&ev)) { edit(Box::new(move |s| s.generation_days = n as u32)) } />"days"</label>
                            <label class="flex items-center gap-1">"Start over past"
                                <input type="number" min="1" class=format!("{INPUT} w-20!") prop:value=s.spool_limit_mb.to_string()
                                    on:change=move |ev| if let Some(n) = number(event_target_value(&ev)) { edit(Box::new(move |s| s.spool_limit_mb = n)) } />"MB unsent"</label>
                            <label class="flex items-center gap-1">"Tell me when"
                                <input type="number" min="1" class=format!("{INPUT} w-16!") prop:value=s.behind_alert_minutes.to_string()
                                    on:change=move |ev| if let Some(n) = number(event_target_value(&ev)) { edit(Box::new(move |s| s.behind_alert_minutes = n as u32)) } />"min behind"</label>
                            <label class="flex items-center gap-1">"Test restore every"
                                <input type="number" min="0" class=format!("{INPUT} w-16!") prop:value=s.test_days.to_string()
                                    on:change=move |ev| if let Some(n) = number(event_target_value(&ev)) { edit(Box::new(move |s| s.test_days = n as u32)) } />"days"</label>
                        </div>
                        <div class="flex items-center gap-2">
                            <button class=BUTTON on:click=save>"Save"</button>
                            {move || saved.get().then(|| view! { <span class="text-sm text-muted">"Saved."</span> })}
                        </div>
                    </div>
                }
            })}
            <div class="flex flex-wrap items-center gap-2 border-t border-zinc-200 pt-3 text-sm dark:border-zinc-800">
                <span class="text-muted">"Test restores decrypt with your key if you keep it here:"</span>
                <input type="password" class=format!("{INPUT} w-64!") placeholder="c6key1…" prop:value=move || restore_key.get()
                    on:input=move |ev| restore_key.set(event_target_value(&ev)) />
                <button class=BUTTON_QUIET on:click=keep_restore_key>"Keep"</button>
                {move || status.get().map(|s| if s.restore_key { "(one is kept)" } else { "(none kept: tests check every object is there)" })}
            </div>
            <div class="space-y-2 border-t border-zinc-200 pt-3 dark:border-zinc-800">
                <div class="flex flex-wrap gap-2">
                    <button class=BUTTON_QUIET disabled=move || busy.get() on:click=move |_| act("run")>"Send now"</button>
                    <button class=BUTTON_QUIET disabled=move || busy.get() on:click=move |_| act("base")>"New base copies now"</button>
                    <button class=BUTTON_QUIET disabled=move || busy.get() on:click=move |_| act("test")>"Test restore now"</button>
                    {move || busy.get().then(|| view! { <span class="text-sm text-muted">"Working…"</span> })}
                </div>
                {move || status.get().map(|s| s.destinations.into_iter().map(|d| {
                    let ok = d.last_error.is_empty();
                    view! {
                        <div class="rounded-md border border-zinc-200 px-3 py-2 text-sm dark:border-zinc-800">
                            <div class="flex flex-wrap gap-x-3">
                                <span class="font-medium">{d.name.clone()}</span>
                                <span class=if ok { "text-emerald-700 dark:text-emerald-400" } else { "text-red-600" }>
                                    {match (ok, d.behind_ms) {
                                        (false, _) => "failing".to_owned(),
                                        (true, Some(ms)) => format!("{} behind", crate::when::compact(ms)),
                                        (true, None) => "up to date".to_owned(),
                                    }}
                                </span>
                                <span class="text-muted">{d.last_ok_at.map(|t| format!("last sent {} ago", crate::when::compact(crate::util::now_ms() - t))).unwrap_or_else(|| "never sent".into())}</span>
                                <span class="text-muted">{format!("keeps {} in {} objects; {} sent since start", size(d.stored_bytes), d.stored_objects, size(d.sent_bytes))}</span>
                            </div>
                            {(!ok).then(|| view! { <p class="text-xs text-red-600">{d.last_error.clone()}</p> })}
                            <p class="text-xs text-muted">
                                {d.generations.iter().map(|g| format!("{}: base copy {} ago{}, {} changes since", g.database, crate::when::compact(crate::util::now_ms() - g.base_at), if g.complete { "" } else { " (not complete)" }, g.segments)).collect::<Vec<_>>().join(" · ")}
                            </p>
                            {d.last_test.map(|t| view! {
                                <p class=if t.ok { "text-xs text-muted" } else { "text-xs text-red-600" }>
                                    {format!("Test restore {} ago: {}{}", crate::when::compact(crate::util::now_ms() - t.at), if t.ok { "fine" } else { "FAILED" }, if t.decrypted { " (decrypted and checked)" } else { " (objects checked, not decrypted)" })}
                                    " — " {t.detail}
                                </p>
                            })}
                        </div>
                    }
                }).collect_view())}
            </div>
        </section>
    }
}
