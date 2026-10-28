//! HTTP and WebSocket access to the head.

use std::time::Duration;

use c6_api::{
    Connection, CreateConnection, CreateEnrollment, CreateSession, Enrollment, LiveEvent, QueuedInput, SendMessage, Session, SessionDetail,
    StreamMsg, Worker,
};
use futures::StreamExt;
use futures::future::{AbortHandle, abortable};
use gloo_net::http::{RequestBuilder, Response};
use gloo_net::websocket::Message;
use gloo_net::websocket::futures::WebSocket;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Fleet and session lists kept current over `WS /api/v1/live`.
#[derive(Clone, Copy)]
pub struct LiveFeed {
    pub workers: ReadSignal<Vec<Worker>>,
    pub sessions: ReadSignal<Vec<Session>>,
    pub connected: ReadSignal<bool>,
}

impl LiveFeed {
    pub fn connect() -> Self {
        let (workers, set_workers) = signal(Vec::new());
        let (sessions, set_sessions) = signal(Vec::new());
        let (connected, set_connected) = signal(false);
        spawn_local(async move {
            let mut backoff = 500;
            loop {
                if let Ok(ws) = WebSocket::open(&ws_url("/api/v1/live")) {
                    let (_tx, mut rx) = ws.split();
                    while let Some(Ok(msg)) = rx.next().await {
                        let Message::Text(text) = msg else { continue };
                        match serde_json::from_str::<LiveEvent>(&text) {
                            Ok(ev) => {
                                set_connected.set(true);
                                backoff = 500;
                                match ev {
                                    LiveEvent::Workers { workers } => set_workers.set(workers),
                                    LiveEvent::Sessions { sessions } => set_sessions.set(sessions),
                                }
                            }
                            Err(e) => leptos::logging::warn!("bad live event: {e}"),
                        }
                    }
                }
                set_connected.set(false);
                gloo_timers::future::sleep(Duration::from_millis(backoff)).await;
                backoff = (backoff * 2).min(10_000);
            }
        });
        Self { workers, sessions, connected }
    }

    pub fn worker_name(&self, id: &str) -> String {
        self.workers.with(|ws| ws.iter().find(|w| w.id.to_string() == id).map(|w| w.name.clone()).unwrap_or_else(|| "unknown".into()))
    }
}

pub fn live() -> LiveFeed {
    expect_context::<LiveFeed>()
}

/// Streams a session's events and live deltas, resuming from the last seen
/// seq after reconnects. Returns a handle that stops it.
pub fn stream_session(session_id: String, on_msg: impl Fn(StreamMsg) + 'static, last_seq: impl Fn() -> u64 + 'static) -> AbortHandle {
    let (fut, handle) = abortable(async move {
        let mut backoff = 500;
        loop {
            let url = ws_url(&format!("/api/v1/sessions/{session_id}/stream?after={}", last_seq()));
            if let Ok(ws) = WebSocket::open(&url) {
                let (_tx, mut rx) = ws.split();
                while let Some(Ok(msg)) = rx.next().await {
                    let Message::Text(text) = msg else { continue };
                    match serde_json::from_str::<StreamMsg>(&text) {
                        Ok(m) => {
                            backoff = 500;
                            on_msg(m);
                        }
                        Err(e) => leptos::logging::warn!("bad stream message: {e}"),
                    }
                }
            }
            gloo_timers::future::sleep(Duration::from_millis(backoff)).await;
            backoff = (backoff * 2).min(10_000);
        }
    });
    spawn_local(async move {
        let _ = fut.await;
    });
    handle
}

pub fn ws_url(path: &str) -> String {
    let loc = window().location();
    let scheme = if loc.protocol().as_deref() == Ok("https:") { "wss" } else { "ws" };
    format!("{scheme}://{}{}", loc.host().unwrap_or_default(), url(path))
}

/// An API address for what can't send headers (WebSockets, images,
/// downloads): it says where this page is in the query.
pub fn url(path: &str) -> String {
    match crate::place::header() {
        Some(p) => format!("{path}{}c6_realm={}", if path.contains('?') { '&' } else { '?' }, enc(&p)),
        None => path.to_owned(),
    }
}

/// Requests that say where this page is (`X-C6-Realm`, docs/realms-ux.md
/// §3): the head keeps them there.
pub(crate) struct Request;

impl Request {
    pub(crate) fn get(url: &str) -> RequestBuilder {
        placed(gloo_net::http::Request::get(url))
    }
    pub(crate) fn post(url: &str) -> RequestBuilder {
        placed(gloo_net::http::Request::post(url))
    }
    pub(crate) fn put(url: &str) -> RequestBuilder {
        placed(gloo_net::http::Request::put(url))
    }
    pub(crate) fn patch(url: &str) -> RequestBuilder {
        placed(gloo_net::http::Request::patch(url))
    }
    pub(crate) fn delete(url: &str) -> RequestBuilder {
        placed(gloo_net::http::Request::delete(url))
    }
}

fn placed(r: RequestBuilder) -> RequestBuilder {
    match crate::place::header() {
        Some(p) => r.header("X-C6-Realm", &p),
        None => r,
    }
}

/// The realm a session is in (for links from before realms had pages).
pub async fn locate_session(id: &str) -> Result<String, String> {
    let v: serde_json::Value = json(Request::get(&format!("/api/v1/locate/sessions/{}", enc(id))).send().await).await?;
    v["realm"].as_str().map(str::to_owned).ok_or_else(|| "no realm".into())
}

/// What waits on you in each realm (for the switcher).
pub async fn elsewhere() -> Result<Vec<c6_api::RealmWaiting>, String> {
    json(Request::get("/api/v1/elsewhere").send().await).await
}

thread_local! {
    /// Set when the head answers 401: the session ended (expired, revoked,
    /// signed out elsewhere). The app then shows the sign-in page.
    static SIGNED_OUT: RwSignal<bool> = RwSignal::new(false);
}

pub fn signed_out() -> RwSignal<bool> {
    SIGNED_OUT.with(|s| *s)
}

thread_local! {
    /// The realm you are looking at ("": every realm), remembered in this
    /// browser (docs/os.md §3). It has an owner of its own, so it lives as
    /// long as the page, whichever component asks for it first.
    static REALM: (Owner, RwSignal<String>) = {
        let owner = Owner::new_root(None);
        // This page's realm; the console's is root (Settings there edits
        // the defaults every realm inherits).
        let signal = owner.with(|| RwSignal::new(crate::place::realm().unwrap_or_else(|| "root".into())));
        (owner, signal)
    };
}

/// The realm you are looking at ("": every realm).
pub fn realm() -> RwSignal<String> {
    REALM.with(|r| r.1)
}

thread_local! {
    /// Bumped when realms are made, renamed or removed (here, or by an app
    /// you approve), so every list of them reloads.
    static REALMS_CHANGED: (Owner, RwSignal<u32>) = {
        let owner = Owner::new_root(None);
        let signal = owner.with(|| RwSignal::new(0));
        (owner, signal)
    };
}

pub fn realms_changed() -> RwSignal<u32> {
    REALMS_CHANGED.with(|r| r.1)
}

fn bump_realms() {
    realms_changed().update(|n| *n += 1);
}

/// `?realm=…` for the realm you are looking at, or "".
fn in_realm(sep: char) -> String {
    let r = realm().get_untracked();
    if r.is_empty() { String::new() } else { format!("{sep}realm={}", enc(&r)) }
}

/// Importing sessions from Claude Code and Codex (docs/imports.md).
pub async fn session_imports() -> Result<c6_api::SessionImports, String> {
    json(Request::get("/api/v1/settings/session_imports").send().await).await
}

pub async fn set_session_imports(s: c6_api::SessionImports) -> Result<(), String> {
    empty(Request::put("/api/v1/settings/session_imports").json(&s).map_err(|e| e.to_string())?.send().await).await
}

/// What was imported: sessions by source and realm.
pub async fn imports() -> Result<serde_json::Value, String> {
    json(Request::get("/api/v1/imports").send().await).await
}

/// The realms (it reloads when they change: track `realms_changed`).
pub async fn realms() -> Result<Vec<c6_api::Realm>, String> {
    json(Request::get("/api/v1/realms").send().await).await
}

pub async fn create_realm(r: c6_api::RealmIn) -> Result<c6_api::Realm, String> {
    let made = send_json(Request::post("/api/v1/realms"), &r).await;
    bump_realms();
    made
}

pub async fn update_realm(id: String, r: c6_api::RealmIn) -> Result<c6_api::Realm, String> {
    let done = send_json(Request::patch(&format!("/api/v1/realms/{}", enc(&id))), &r).await;
    bump_realms();
    done
}

pub async fn delete_realm(id: String) -> Result<(), String> {
    let done = empty(Request::delete(&format!("/api/v1/realms/{}", enc(&id))).send().await).await;
    bump_realms();
    done
}

// --- The realm explorer (docs/explorer.md) -----------------------------------

pub async fn explorer_map() -> Result<serde_json::Value, String> {
    json(Request::get("/api/v1/explorer/map").send().await).await
}

/// A settings list as a realm uses it: each entry with where it comes from
/// (`root`, `realm`, `app:<name>`).
pub async fn effective_list(realm: String, key: String) -> Result<Vec<serde_json::Value>, String> {
    json(Request::get(&format!("/api/v1/realms/{}/effective/{}", enc(&realm), enc(&key))).send().await).await
}

/// Whether this realm sets a per-realm setting itself (else root's applies).
pub async fn own_setting(name: &'static str) -> Result<bool, String> {
    let v: serde_json::Value = json(Request::get(&format!("/api/v1/settings/own/{name}")).send().await).await?;
    Ok(v["own"] == true)
}

/// This realm goes back to following root's value.
pub async fn drop_own_setting(name: &'static str) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/settings/own/{name}")).send().await).await
}

pub async fn realm_summary(realm: String) -> Result<c6_api::RealmSummary, String> {
    json(Request::get(&format!("/api/v1/realms/{}/summary", enc(&realm))).send().await).await
}

pub async fn realm_activity(realm: String, since: i64) -> Result<Vec<c6_api::ExRef>, String> {
    json(Request::get(&format!("/api/v1/realms/{}/activity?since={since}&limit=60", enc(&realm))).send().await).await
}

pub async fn realm_list(realm: String, section: String, q: String) -> Result<Vec<c6_api::ExRef>, String> {
    json(Request::get(&format!("/api/v1/realms/{}/list?section={}&q={}", enc(&realm), enc(&section), enc(&q))).send().await).await
}

pub async fn realm_search(realm: String, q: String) -> Result<Vec<c6_api::ExGroup>, String> {
    json(Request::get(&format!("/api/v1/realms/{}/search?q={}", enc(&realm), enc(&q))).send().await).await
}

pub async fn realm_neighborhood(realm: String, kind: String, id: String) -> Result<serde_json::Value, String> {
    json(Request::get(&format!("/api/v1/realms/{}/neighborhood?kind={}&id={}", enc(&realm), enc(&kind), enc(&id))).send().await).await
}

pub async fn realm_object(realm: String, kind: String, id: String) -> Result<c6_api::ExObject, String> {
    json(Request::get(&format!("/api/v1/realms/{}/object?kind={}&id={}", enc(&realm), enc(&kind), enc(&id))).send().await).await
}

pub async fn packages() -> Result<Vec<c6_api::PackageInfo>, String> {
    json(Request::get("/api/v1/packages").send().await).await
}

pub async fn add_package(p: c6_api::PackageIn) -> Result<c6_api::PackageInfo, String> {
    send_json(Request::post("/api/v1/packages"), &p).await
}

pub async fn installs() -> Result<Vec<c6_api::Install>, String> {
    json(Request::get("/api/v1/installs").send().await).await
}

pub async fn install_package(i: c6_api::InstallIn) -> Result<c6_api::Install, String> {
    send_json(Request::post("/api/v1/installs"), &i).await
}

pub async fn upgrade_install(id: i64) -> Result<c6_api::Install, String> {
    send_json(Request::post(&format!("/api/v1/installs/{id}/upgrade")), &serde_json::json!({})).await
}

pub async fn remove_install(id: i64) -> Result<c6_api::Install, String> {
    json(Request::delete(&format!("/api/v1/installs/{id}")).send().await).await
}

/// Where a realm's package downloads from.
/// An app instance's package (`fork`: as a new package of that name, to
/// change and install), with its data or not.
pub fn export_url(realm: &str, instance: &str, fork: &str, data: bool) -> String {
    format!(
        "/api/v1/apps/export?realm={}&instance={}{}{}",
        enc(realm),
        enc(instance),
        if fork.is_empty() { String::new() } else { format!("&fork={}", enc(fork)) },
        if data { "&data=true" } else { "" }
    )
}

/// The accounts connected for an app's OAuth providers.
pub async fn app_accounts(realm: &str, instance: &str) -> Result<Vec<c6_api::OAuthAccount>, String> {
    json(Request::get(&format!("/api/v1/apps/{}/{}/accounts", enc(realm), enc(instance))).send().await).await
}

/// Starts connecting an account: where to send you.
pub async fn connect_account(realm: &str, instance: &str, provider: &str) -> Result<String, String> {
    let v: serde_json::Value = send_json(
        Request::post(&format!("/api/v1/apps/{}/{}/providers/{}/connect", enc(realm), enc(instance), enc(provider))),
        &serde_json::json!({}),
    )
    .await?;
    Ok(v["authorize_url"].as_str().unwrap_or_default().to_owned())
}

pub async fn remove_account(realm: &str, instance: &str, id: &str) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/apps/{}/{}/accounts/{}", enc(realm), enc(instance), enc(id))).send().await).await
}

/// An app instance's operations (docs/apps.md §7).
pub async fn operations(realm: &str, instance: &str) -> Result<Vec<c6_api::Operation>, String> {
    json(Request::get(&format!("/api/v1/apps/{}/{}/operations", enc(realm), enc(instance))).send().await).await
}

pub async fn call_operation(realm: &str, instance: &str, name: &str, input: serde_json::Value) -> Result<serde_json::Value, String> {
    send_json(
        Request::post(&format!("/api/v1/apps/{}/{}/operations/{}", enc(realm), enc(instance), enc(name))),
        &serde_json::json!({"input": input}),
    )
    .await
}

pub async fn set_install_settings(id: i64, values: serde_json::Value) -> Result<c6_api::Install, String> {
    send_json(Request::put(&format!("/api/v1/installs/{id}/settings")), &values).await
}

pub async fn grants() -> Result<Vec<c6_api::Grant>, String> {
    json(Request::get("/api/v1/grants").send().await).await
}

pub async fn create_grant(g: c6_api::GrantIn) -> Result<c6_api::Grant, String> {
    send_json(Request::post("/api/v1/grants"), &g).await
}

pub async fn revoke_grant(id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/grants/{}", enc(&id))).send().await).await
}

pub async fn grant_uses(id: String) -> Result<Vec<c6_api::GrantUse>, String> {
    json(Request::get(&format!("/api/v1/grants/{}/uses", enc(&id))).send().await).await
}

/// Which realms a worker ("workers") or connection ("connections") serves.
pub async fn set_availability(what: &'static str, id: String, realms: Vec<String>) -> Result<(), String> {
    empty(Request::put(&format!("/api/v1/{what}/{id}/realms")).json(&c6_api::RealmsIn { realms }).map_err(|e| e.to_string())?.send().await)
        .await
}

async fn check(resp: Result<Response, gloo_net::Error>) -> Result<Response, String> {
    let resp = resp.map_err(|e| e.to_string())?;
    if resp.ok() {
        return Ok(resp);
    }
    if resp.status() == 401 {
        signed_out().set(true);
    }
    let msg = resp.json::<c6_api::ApiError>().await.map(|e| e.error).unwrap_or_else(|_| resp.status_text());
    Err(msg)
}

async fn json<T: DeserializeOwned>(req: Result<Response, gloo_net::Error>) -> Result<T, String> {
    check(req).await?.json().await.map_err(|e| e.to_string())
}

async fn send_json<T: DeserializeOwned>(req: RequestBuilder, body: &impl Serialize) -> Result<T, String> {
    json(req.json(body).map_err(|e| e.to_string())?.send().await).await
}

async fn empty(req: Result<Response, gloo_net::Error>) -> Result<(), String> {
    check(req).await.map(drop)
}

pub async fn create_enrollment(name: Option<String>) -> Result<Enrollment, String> {
    send_json(Request::post("/api/v1/workers/enrollments"), &CreateEnrollment { name }).await
}

pub async fn revoke_worker(id: String) -> Result<(), String> {
    empty(Request::post(&format!("/api/v1/workers/{id}/revoke")).send().await).await
}

pub async fn delete_worker(id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/workers/{id}")).send().await).await
}

pub async fn connections() -> Result<Vec<Connection>, String> {
    json(Request::get("/api/v1/connections").send().await).await
}

pub async fn create_connection(req: CreateConnection) -> Result<Connection, String> {
    send_json(Request::post("/api/v1/connections"), &req).await
}

/// Starts signing in with ChatGPT: a new account, or `connection_id`'s
/// account again.
pub async fn start_chatgpt(
    name: String,
    connection_id: Option<String>,
    login_hint: Option<String>,
) -> Result<c6_api::ChatGptSignIn, String> {
    send_json(Request::post("/api/v1/connections/chatgpt/start"), &c6_api::StartChatGptSignIn { name, connection_id, login_hint }).await
}

pub async fn complete_chatgpt(callback_url: String) -> Result<Connection, String> {
    send_json(Request::post("/api/v1/connections/chatgpt/complete"), &c6_api::CompleteChatGptSignIn { callback_url }).await
}

pub async fn delete_connection(id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/connections/{id}")).send().await).await
}

pub async fn models(connection_id: String) -> Result<Vec<String>, String> {
    json(Request::get(&format!("/api/v1/connections/{connection_id}/models")).send().await).await
}

pub async fn create_session(req: CreateSession) -> Result<Session, String> {
    send_json(Request::post("/api/v1/sessions"), &req).await
}

pub async fn create_side_chat(parent: String, req: c6_api::CreateSideChat) -> Result<Session, String> {
    send_json(Request::post(&format!("/api/v1/sessions/{parent}/side-chats")), &req).await
}

pub async fn session(id: String) -> Result<SessionDetail, String> {
    json(Request::get(&format!("/api/v1/sessions/{id}")).send().await).await
}

/// `switch`: (connection, model, effort) to switch the session to from this
/// message; an empty effort is the model's default.
pub async fn send_message(
    id: String,
    text: String,
    switch: Option<(String, String, String)>,
    attachments: Vec<c6_api::AttachmentUpload>,
) -> Result<QueuedInput, String> {
    send_message_waking(id, text, switch, attachments, false).await
}

/// `wake`: to a subagent, start a turn in its idle parent when its report
/// comes.
pub async fn send_message_waking(
    id: String,
    text: String,
    switch: Option<(String, String, String)>,
    attachments: Vec<c6_api::AttachmentUpload>,
    wake: bool,
) -> Result<QueuedInput, String> {
    let (connection_id, model, effort) = match switch {
        Some((c, m, e)) => (Some(c), Some(m), Some(e).filter(|e| !e.is_empty())),
        None => (None, None, None),
    };
    send_json(
        Request::post(&format!("/api/v1/sessions/{id}/messages")),
        &SendMessage { text, attachments, connection_id, model, effort, wake },
    )
    .await
}

/// Continues a subagent whose last turn did not finish.
pub async fn resume(id: String, wake: bool) -> Result<QueuedInput, String> {
    send_json(Request::post(&format!("/api/v1/sessions/{id}/resume")), &c6_api::ResumeRequest { wake }).await
}

pub async fn interrupt(id: String) -> Result<(), String> {
    empty(Request::post(&format!("/api/v1/sessions/{id}/interrupt")).send().await).await
}

pub async fn archive(id: String) -> Result<(), String> {
    #[derive(Serialize)]
    struct Body {
        archived: bool,
    }
    empty(Request::patch(&format!("/api/v1/sessions/{id}")).json(&Body { archived: true }).map_err(|e| e.to_string())?.send().await).await
}

pub async fn dismiss_rejected(id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/sessions/{id}/queue/rejected")).send().await).await
}

// --- Worker files, git, terminals ---------------------------------------------

fn enc(s: &str) -> String {
    js_sys::encode_uri_component(s).as_string().unwrap_or_default()
}

pub async fn fs_list(worker: String, path: String, hidden: bool) -> Result<serde_json::Value, String> {
    json(Request::get(&format!("/api/v1/workers/{worker}/fs/list?path={}&hidden={hidden}", enc(&path))).send().await).await
}

/// A folder on the head's own disk (its home folder when `path` is empty).
pub async fn head_fs_list(path: String, hidden: bool) -> Result<serde_json::Value, String> {
    json(Request::get(&format!("/api/v1/head/fs/list?path={}&hidden={hidden}", enc(&path))).send().await).await
}

/// A text file on the head's own disk.
pub async fn head_fs_read(path: String) -> Result<serde_json::Value, String> {
    json(Request::get(&format!("/api/v1/head/fs/read?path={}", enc(&path))).send().await).await
}

pub async fn fs_read(worker: String, path: String) -> Result<serde_json::Value, String> {
    json(Request::get(&format!("/api/v1/workers/{worker}/fs/read?path={}", enc(&path))).send().await).await
}

pub async fn git_status(worker: String, path: String) -> Result<serde_json::Value, String> {
    json(Request::get(&format!("/api/v1/workers/{worker}/git/status?path={}", enc(&path))).send().await).await
}

pub async fn git_diff(worker: String, path: String) -> Result<serde_json::Value, String> {
    json(Request::get(&format!("/api/v1/workers/{worker}/git/diff?path={}", enc(&path))).send().await).await
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct TerminalInfo {
    pub id: String,
    pub cwd: String,
    pub exited: Option<i32>,
}

pub async fn terminals(worker: String) -> Result<Vec<TerminalInfo>, String> {
    json(Request::get(&format!("/api/v1/workers/{worker}/terminals")).send().await).await
}

/// A terminal on a worker, or (`ssh`) on an SSH target through it.
pub async fn open_terminal(worker: String, cwd: String, ssh: Option<String>) -> Result<TerminalInfo, String> {
    send_json(Request::post(&format!("/api/v1/workers/{worker}/terminals")), &serde_json::json!({"cwd": cwd, "ssh": ssh})).await
}

pub async fn close_terminal(worker: String, id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/workers/{worker}/terminals/{id}")).send().await).await
}

pub async fn children(id: String) -> Result<Vec<Session>, String> {
    json(Request::get(&format!("/api/v1/sessions/{id}/children")).send().await).await
}

/// Everything below a session (side chats and subagents, at any depth).
pub async fn descendants(id: String) -> Result<Vec<Session>, String> {
    json(Request::get(&format!("/api/v1/sessions/{id}/children?tree=1")).send().await).await
}

pub async fn get_setting(name: &'static str) -> Result<serde_json::Value, String> {
    json(Request::get(&format!("/api/v1/settings/{name}{}", in_realm('?'))).send().await).await
}

pub async fn analytics(days: u32, labels: Vec<String>, by: String) -> Result<c6_api::AnalyticsReport, String> {
    let (labels, by) = (js_sys::encode_uri_component(&labels.join(",")), js_sys::encode_uri_component(&by));
    json(Request::get(&format!("/api/v1/analytics?days={days}&labels={labels}&by={by}")).send().await).await
}

pub async fn usage(days: u32, labels: Vec<String>) -> Result<c6_api::UsageReport, String> {
    let labels = js_sys::encode_uri_component(&labels.join(","));
    json(Request::get(&format!("/api/v1/usage?days={days}&labels={labels}")).send().await).await
}

pub async fn prompts() -> Result<Vec<c6_api::PromptInfo>, String> {
    json(Request::get("/api/v1/settings/prompts").send().await).await
}

pub async fn set_prompt(name: String, body: String) -> Result<(), String> {
    match Request::put(&format!("/api/v1/settings/prompts/{}", enc(&name))).json(&c6_api::PromptIn { body }) {
        Ok(r) => empty(r.send().await).await,
        Err(e) => Err(e.to_string()),
    }
}

/// The version the head runs.
pub async fn version() -> Result<String, String> {
    let v: serde_json::Value = json(Request::get("/api/v1/version").send().await).await?;
    Ok(v["version"].as_str().unwrap_or_default().to_owned())
}

/// Updating carbon6 from GitHub releases (docs/updates.md).
pub async fn updates() -> Result<c6_api::Updates, String> {
    json(Request::get("/api/v1/system/updates").send().await).await
}

/// The activity panel: model calls, sessions running, connectors' syncs.
pub async fn activity() -> Result<serde_json::Value, String> {
    json(Request::get("/api/v1/system/activity").send().await).await
}

pub async fn activity_counts() -> Result<serde_json::Value, String> {
    json(Request::get("/api/v1/system/activity/counts").send().await).await
}

pub async fn activity_call(id: u64) -> Result<serde_json::Value, String> {
    json(Request::get(&format!("/api/v1/system/activity/calls/{id}")).send().await).await
}

pub async fn check_updates() -> Result<c6_api::Updates, String> {
    send_json(Request::post("/api/v1/system/updates/check"), &serde_json::json!({})).await
}

/// `what`: apply or skip.
pub async fn update_release(version: String, what: &'static str) -> Result<c6_api::Updates, String> {
    send_json(Request::post(&format!("/api/v1/system/updates/{version}/{what}")), &serde_json::json!({})).await
}

pub async fn set_update_settings(s: c6_api::UpdateSettings) -> Result<(), String> {
    empty(Request::put("/api/v1/settings/updates").json(&s).map_err(|e| e.to_string())?.send().await).await
}

pub async fn upgrades() -> Result<Vec<c6_api::Upgrade>, String> {
    json(Request::get("/api/v1/system/upgrades").send().await).await
}

pub async fn hosts() -> Result<Vec<c6_api::HostStatus>, String> {
    json(Request::get("/api/v1/hosts").send().await).await
}

pub async fn check_host(name: String) -> Result<c6_api::HostCheck, String> {
    json(Request::post(&format!("/api/v1/hosts/{}/check", js_sys::encode_uri_component(&name))).send().await).await
}

pub async fn workflows() -> Result<Vec<c6_api::Workflow>, String> {
    json(Request::get("/api/v1/settings/workflows").send().await).await
}

pub async fn workflow_runs() -> Result<Vec<c6_api::WorkflowRun>, String> {
    json(Request::get("/api/v1/workflows/runs?limit=15").send().await).await
}

pub async fn run_workflow(name: String, input: String) -> Result<c6_api::WorkflowRun, String> {
    let body = c6_api::RunWorkflow { input };
    send_json(Request::post(&format!("/api/v1/workflows/{}/run", js_sys::encode_uri_component(&name))), &body).await
}

pub async fn decide_workflow_step(run: String, step: String, allow: bool) -> Result<(), String> {
    let body = c6_api::ApprovalDecision { allow };
    empty(
        Request::post(&format!("/api/v1/workflows/runs/{run}/steps/{}/decide", js_sys::encode_uri_component(&step)))
            .json(&body)
            .map_err(|e| e.to_string())?
            .send()
            .await,
    )
    .await
}

pub async fn push_key() -> Result<c6_api::PushKey, String> {
    json(Request::get("/api/v1/push/key").send().await).await
}

pub async fn push_devices() -> Result<Vec<c6_api::PushDevice>, String> {
    json(Request::get("/api/v1/push/subscriptions").send().await).await
}

pub async fn push_subscribe(s: c6_api::PushSubscription) -> Result<c6_api::PushDevice, String> {
    send_json(Request::post("/api/v1/push/subscriptions"), &s).await
}

pub async fn push_unsubscribe(id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/push/subscriptions/{id}")).send().await).await
}

pub async fn memex_status() -> Result<c6_api::MemexStatus, String> {
    json(Request::get("/api/v1/memex/status").send().await).await
}

pub async fn memex_search(q: String, kinds: Vec<String>) -> Result<Vec<c6_api::MemexHit>, String> {
    let url = format!(
        "/api/v1/memex/search?q={}&kinds={}&limit=30",
        js_sys::encode_uri_component(&q),
        js_sys::encode_uri_component(&kinds.join(","))
    );
    json(Request::get(&url).send().await).await
}

/// An item (`{"item": …}`) or a thing (`{"thing": …}`).
pub async fn memex_get(id: String) -> Result<serde_json::Value, String> {
    json(Request::get(&format!("/api/v1/memex/get?id={}", js_sys::encode_uri_component(&id))).send().await).await
}

pub async fn memex_note(text: String, about: Vec<String>) -> Result<serde_json::Value, String> {
    send_json(Request::post("/api/v1/memex/notes"), &c6_api::MemexNoteIn { text, about }).await
}

/// Mail, calendars and contacts: a memex method (see the head's /memex/rpc).
/// Mail, calendars and contacts are the realm's you look at (each realm
/// has its own); the rest is home's unless a realm's token says otherwise.
pub async fn rpc(method: &str, params: serde_json::Value) -> Result<serde_json::Value, String> {
    let area = method.split('.').nth(1).unwrap_or_default();
    let per_realm = matches!(area, "mail" | "calendar" | "contacts" | "google" | "google_sync" | "feeds" | "chat");
    let url = format!("/api/v1/memex/rpc{}", if per_realm { in_realm('?') } else { String::new() });
    send_json(Request::post(&url), &serde_json::json!({"method": method, "params": params})).await
}

/// A mail attachment's address, in the realm you look at.
pub fn mail_attachment(item: &str, index: u32) -> String {
    format!("/api/v1/mail/attachment?item={}&index={index}{}", enc(item), in_realm('&'))
}

pub async fn rpc_as<T: serde::de::DeserializeOwned>(method: &str, params: serde_json::Value) -> Result<T, String> {
    serde_json::from_value(rpc(method, params).await?).map_err(|e| e.to_string())
}

/// Slack (docs/slack.md): the realm's setup and workspaces.
pub async fn slack_setup() -> Result<c6_api::SlackSetup, String> {
    json(Request::get("/api/v1/slack").send().await).await
}

pub async fn slack_client(client_id: String, client_secret: Option<String>) -> Result<(), String> {
    let body = c6_api::SlackClientIn { client_id, client_secret };
    empty(Request::put("/api/v1/slack/client").json(&body).map_err(|e| e.to_string())?.send().await).await
}

pub async fn slack_install() -> Result<serde_json::Value, String> {
    json(Request::post("/api/v1/slack/install").send().await).await
}

/// Where to send you to connect a workspace.
pub async fn slack_connect() -> Result<String, String> {
    let v: serde_json::Value = json(Request::post("/api/v1/slack/connect").send().await).await?;
    v["authorize_url"].as_str().map(str::to_owned).ok_or_else(|| "no address".into())
}

pub async fn slack_sync() -> Result<(), String> {
    empty(Request::post("/api/v1/slack/sync").send().await).await
}

pub async fn slack_remove(id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/slack/accounts/{}", enc(&id))).send().await).await
}

pub async fn google_client() -> Result<c6_api::GoogleClient, String> {
    json(Request::get(&format!("/api/v1/google/client{}", in_realm('?'))).send().await).await
}

pub async fn set_google_client(client_id: String, client_secret: Option<String>) -> Result<(), String> {
    let body = c6_api::GoogleClientIn { client_id, client_secret };
    empty(Request::put(&format!("/api/v1/google/client{}", in_realm('?'))).json(&body).map_err(|e| e.to_string())?.send().await).await
}

pub async fn google_connect() -> Result<String, String> {
    let v: serde_json::Value = json(Request::post(&format!("/api/v1/google/connect{}", in_realm('?'))).send().await).await?;
    Ok(v["authorize_url"].as_str().unwrap_or_default().to_owned())
}

/// Asks to install the Gmail, Calendar and Contacts apps.
pub async fn google_install() -> Result<Vec<c6_api::Install>, String> {
    json(Request::post(&format!("/api/v1/google/install{}", in_realm('?'))).send().await).await
}

pub async fn google_accounts() -> Result<Vec<c6_api::GoogleAccount>, String> {
    json(Request::get(&format!("/api/v1/google/accounts{}", in_realm('?'))).send().await).await
}

pub async fn google_remove(id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/google/accounts/{id}{}", in_realm('?'))).send().await).await
}

pub async fn memex_upload(u: c6_api::MemexUpload) -> Result<serde_json::Value, String> {
    send_json(Request::post("/api/v1/memex/upload"), &u).await
}

pub async fn memex_trails() -> Result<Vec<c6_api::MemexTrail>, String> {
    json(Request::get("/api/v1/memex/trails").send().await).await
}

pub async fn memex_trail(id: String) -> Result<c6_api::MemexTrail, String> {
    json(Request::get(&format!("/api/v1/memex/trails/{}", js_sys::encode_uri_component(&id))).send().await).await
}

pub async fn memex_trail_append(trail: String, target: String, comment: String) -> Result<serde_json::Value, String> {
    send_json(Request::post("/api/v1/memex/trails"), &serde_json::json!({"trail": trail, "target": target, "comment": comment})).await
}

pub async fn memex_trail_remove(trail: String, step: i64) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/memex/trails/{trail}/steps/{step}")).send().await).await
}

pub async fn memex_trail_delete(trail: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/memex/trails/{trail}")).send().await).await
}

pub async fn memex_scan() -> Result<serde_json::Value, String> {
    json(Request::post("/api/v1/memex/scan").send().await).await
}

pub async fn memex_mentions(q: String) -> Result<Vec<c6_api::MemexThingRef>, String> {
    json(Request::get(&format!("/api/v1/memex/mentions?q={}", js_sys::encode_uri_component(&q))).send().await).await
}

pub async fn memex_delete(id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/memex/items/{id}")).send().await).await
}

pub async fn test_channel(name: String) -> Result<(), String> {
    let body = c6_api::TestChannel { name };
    empty(Request::post("/api/v1/settings/channels/test").json(&body).map_err(|e| e.to_string())?.send().await).await
}

pub async fn put_setting(name: &'static str, value: serde_json::Value) -> Result<(), String> {
    empty(Request::put(&format!("/api/v1/settings/{name}{}", in_realm('?'))).json(&value).map_err(|e| e.to_string())?.send().await).await
}

pub async fn fs_mkdir(worker: String, path: String) -> Result<serde_json::Value, String> {
    send_json(Request::post(&format!("/api/v1/workers/{worker}/fs/mkdir")), &serde_json::json!({"path": path})).await
}

/// Checks connection details and returns the models the server offers.
pub async fn test_connection(
    kind: String,
    base_url: String,
    api_key: Option<String>,
    connection_id: Option<String>,
) -> Result<Vec<String>, String> {
    let body = serde_json::json!({"kind": kind, "base_url": base_url, "api_key": api_key, "connection_id": connection_id});
    send_json(Request::post("/api/v1/connections/test"), &body).await
}

// --- Sign-in ------------------------------------------------------------------

pub async fn auth_status() -> Result<c6_api::AuthStatus, String> {
    json(Request::get("/api/v1/auth/status").send().await).await
}

pub async fn redeem_link(token: String) -> Result<(), String> {
    empty(Request::post("/api/v1/auth/link").json(&c6_api::RedeemLink { token }).map_err(|e| e.to_string())?.send().await).await
}

pub async fn logout() -> Result<(), String> {
    empty(Request::post("/api/v1/auth/logout").send().await).await
}

pub async fn passkey_options(path: &str) -> Result<serde_json::Value, String> {
    json(Request::post(&format!("/api/v1/auth/passkeys/{path}/begin")).send().await).await
}

pub async fn passkey_login(credential: serde_json::Value) -> Result<(), String> {
    empty(Request::post("/api/v1/auth/passkeys/login/finish").json(&credential).map_err(|e| e.to_string())?.send().await).await
}

pub async fn passkey_register(name: String, credential: serde_json::Value) -> Result<c6_api::Passkey, String> {
    send_json(Request::post("/api/v1/auth/passkeys/register/finish"), &c6_api::RegisterPasskey { name, credential }).await
}

pub async fn passkeys() -> Result<Vec<c6_api::Passkey>, String> {
    json(Request::get("/api/v1/auth/passkeys").send().await).await
}

pub async fn delete_passkey(id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/auth/passkeys/{id}")).send().await).await
}

pub async fn auth_sessions() -> Result<Vec<c6_api::AuthSession>, String> {
    json(Request::get("/api/v1/auth/sessions").send().await).await
}

pub async fn delete_auth_session(id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/auth/sessions/{id}")).send().await).await
}

pub async fn api_tokens() -> Result<Vec<c6_api::ApiToken>, String> {
    json(Request::get("/api/v1/auth/tokens").send().await).await
}

pub async fn create_api_token(name: String, realm: Option<String>) -> Result<c6_api::ApiToken, String> {
    send_json(Request::post("/api/v1/auth/tokens"), &c6_api::CreateApiToken { name, realm }).await
}

pub async fn delete_api_token(id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/auth/tokens/{id}")).send().await).await
}

pub async fn create_login_code() -> Result<c6_api::LoginCode, String> {
    json(Request::post("/api/v1/auth/codes").send().await).await
}

pub async fn redeem_code(code: String) -> Result<(), String> {
    empty(Request::post("/api/v1/auth/code").json(&c6_api::RedeemCode { code }).map_err(|e| e.to_string())?.send().await).await
}

pub async fn rename_passkey(id: String, name: String) -> Result<(), String> {
    empty(
        Request::patch(&format!("/api/v1/auth/passkeys/{id}"))
            .json(&c6_api::RenamePasskey { name })
            .map_err(|e| e.to_string())?
            .send()
            .await,
    )
    .await
}

pub async fn search(q: String, kind: String, worker: String, labels: Vec<String>) -> Result<c6_api::SearchResponse, String> {
    let mut params = vec![("q", q)];
    if !labels.is_empty() {
        params.push(("labels", labels.join(",")));
    }
    if !kind.is_empty() {
        params.push(("kind", kind));
    }
    if !worker.is_empty() {
        params.push(("worker", worker));
    }
    json(Request::get("/api/v1/search").query(params).send().await).await
}

pub async fn session_matches(id: String, q: String) -> Result<c6_api::SessionMatches, String> {
    json(Request::get(&format!("/api/v1/sessions/{id}/matches")).query([("q", q)]).send().await).await
}

pub async fn rewind(id: String, to_seq: u64, restore_files: bool) -> Result<(), String> {
    empty(
        Request::post(&format!("/api/v1/sessions/{id}/rewind"))
            .json(&c6_api::RewindRequest { to_seq, restore_files })
            .map_err(|e| e.to_string())?
            .send()
            .await,
    )
    .await
}

/// Files and folders under `root` matching `q`, best first: (path, is folder).
pub async fn fs_find(worker: String, root: String, q: String) -> Result<Vec<(String, bool)>, String> {
    let v: serde_json::Value =
        json(Request::get(&format!("/api/v1/workers/{worker}/fs/find")).query([("root", root), ("q", q)]).send().await).await?;
    Ok(v["paths"]
        .as_array()
        .map(|a| a.iter().filter_map(|p| Some((p["path"].as_str()?.to_owned(), p["dir"].as_bool().unwrap_or(false)))).collect())
        .unwrap_or_default())
}

/// The slash commands the composer offers in a session (built-in, and the
/// custom ones that apply to it).
pub async fn commands(session: String) -> Result<Vec<c6_api::CommandInfo>, String> {
    json(Request::get("/api/v1/commands").query([("session", session)]).send().await).await
}

/// Replaces the labels you attached to a session.
pub async fn set_colour(id: String, colour: String) -> Result<Session, String> {
    send_json(Request::put(&format!("/api/v1/sessions/{id}/colour")), &c6_api::SetColour { colour }).await
}

/// The connection, model and effort a session's subagents run on from now
/// on (an empty model: the session's own).
pub async fn set_subagent_model(id: String, req: c6_api::SetSubagentModel) -> Result<Session, String> {
    send_json(Request::put(&format!("/api/v1/sessions/{id}/subagent-model")), &req).await
}

pub async fn set_labels(id: String, labels: Vec<String>) -> Result<Session, String> {
    send_json(Request::put(&format!("/api/v1/sessions/{id}/labels")), &c6_api::SetLabels { labels }).await
}

/// What applies to a session (commands, agents, skills, hooks) and why not.
pub async fn session_config(id: String) -> Result<c6_api::EffectiveConfig, String> {
    json(Request::get(&format!("/api/v1/sessions/{id}/config")).send().await).await
}

/// Stops a session's background task.
pub async fn stop_task(id: String, task: String) -> Result<(), String> {
    empty(Request::post(&format!("/api/v1/sessions/{id}/tasks/{task}/stop")).send().await).await
}

/// The realm you look at's secrets (its own and root's), or every realm's.
pub async fn secrets() -> Result<Vec<c6_api::SecretInfo>, String> {
    json(Request::get(&format!("/api/v1/settings/secrets{}", in_realm('?'))).send().await).await
}

/// Sets a secret in this page's realm (root's, in the console).
pub async fn put_secret(name: String, req: c6_api::PutSecret) -> Result<(), String> {
    empty(Request::put(&format!("/api/v1/settings/secrets/{name}{}", in_realm('?'))).json(&req).map_err(|e| e.to_string())?.send().await)
        .await
}

pub async fn delete_secret(name: String, realm: String) -> Result<(), String> {
    let q = if realm.is_empty() { String::new() } else { format!("?realm={}", enc(&realm)) };
    empty(Request::delete(&format!("/api/v1/settings/secrets/{name}{q}")).send().await).await
}

pub async fn templates() -> Result<Vec<c6_api::Template>, String> {
    json(Request::get("/api/v1/settings/templates").send().await).await
}

/// Every template you can run, with its realm.
pub async fn all_templates() -> Result<Vec<c6_api::RealmTemplate>, String> {
    json(Request::get("/api/v1/templates").send().await).await
}

pub async fn run_template(name: String, input: String, realm: Option<String>) -> Result<c6_api::TemplateRun, String> {
    send_json(Request::post(&format!("/api/v1/templates/{name}/run")), &c6_api::RunTemplate { input, realm }).await
}

pub async fn automations() -> Result<Vec<c6_api::Automation>, String> {
    json(Request::get(&format!("/api/v1/automations{}", in_realm('?'))).send().await).await
}

pub async fn create_automation(e: c6_api::AutomationEdit) -> Result<c6_api::Automation, String> {
    send_json(Request::post(&format!("/api/v1/automations{}", in_realm('?'))), &e).await
}

pub async fn update_automation(id: String, e: c6_api::AutomationEdit) -> Result<c6_api::Automation, String> {
    send_json(Request::patch(&format!("/api/v1/automations/{id}")), &e).await
}

pub async fn delete_automation(id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/automations/{id}")).send().await).await
}

pub async fn run_automation(id: String) -> Result<c6_api::TemplateRun, String> {
    json(Request::post(&format!("/api/v1/automations/{id}/run")).send().await).await
}

pub async fn automation_runs(id: String) -> Result<Vec<c6_api::AutomationRun>, String> {
    json(Request::get(&format!("/api/v1/automations/{id}/runs?limit=20")).send().await).await
}

pub async fn rotate_automation(id: String) -> Result<c6_api::Automation, String> {
    json(Request::post(&format!("/api/v1/automations/{id}/rotate")).send().await).await
}

pub async fn memories() -> Result<Vec<c6_api::Memory>, String> {
    json(Request::get("/api/v1/memories").send().await).await
}

pub async fn save_memory(m: c6_api::Memory) -> Result<c6_api::Memory, String> {
    if m.id.is_empty() {
        send_json(Request::post("/api/v1/memories"), &m).await
    } else {
        send_json(Request::put(&format!("/api/v1/memories/{}", m.id)), &m).await
    }
}

pub async fn delete_memory(id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/memories/{id}")).send().await).await
}

/// Your answer for a tool call waiting for approval.
pub async fn decide(id: String, call_id: String, allow: bool) -> Result<(), String> {
    let resp = Request::post(&format!("/api/v1/sessions/{id}/approvals/{call_id}"))
        .json(&c6_api::ApprovalDecision { allow })
        .map_err(|e| e.to_string())?
        .send()
        .await;
    empty(resp).await
}

/// Asks the assistant (a new conversation).
pub async fn ask_assistant(prompt: String, context: String) -> Result<c6_api::Session, String> {
    let realm = Some(realm().get_untracked()).filter(|r| !r.is_empty());
    send_json(Request::post("/api/v1/assistant"), &c6_api::AskAssistant { prompt, context, realm }).await
}

/// Your answers to the assistant's question.
pub async fn answer(
    id: String,
    call_id: String,
    values: serde_json::Map<String, serde_json::Value>,
    cancelled: bool,
) -> Result<(), String> {
    empty(
        Request::post(&format!("/api/v1/sessions/{id}/answers/{call_id}"))
            .json(&c6_api::AnswerIn { values, cancelled })
            .map_err(|e| e.to_string())?
            .send()
            .await,
    )
    .await
}

pub async fn decisions(state: String) -> Result<Vec<c6_api::Decision>, String> {
    json(Request::get(&format!("/api/v1/decisions?state={state}")).send().await).await
}

/// What you'd likely type next in a session (docs/suggestions.md); None when there's none.
pub async fn session_suggestion(id: String) -> Result<Option<c6_api::Suggestion>, String> {
    let resp = check(Request::get(&format!("/api/v1/sessions/{id}/suggestion")).send().await).await?;
    if resp.status() == 204 {
        return Ok(None);
    }
    resp.json().await.map(Some).map_err(|e| e.to_string())
}

pub async fn suggestion_settings() -> Result<c6_api::SuggestionSettings, String> {
    json(Request::get(&format!("/api/v1/settings/suggestions{}", in_realm('?'))).send().await).await
}

pub async fn set_suggestion_settings(s: c6_api::SuggestionSettings) -> Result<(), String> {
    empty(Request::put(&format!("/api/v1/settings/suggestions{}", in_realm('?'))).json(&s).map_err(|e| e.to_string())?.send().await).await
}

/// A suggestion was shown, or (`accepted`) taken.
pub async fn suggestion_seen(id: String, for_seq: i64, accepted: bool) {
    let body = c6_api::SuggestionSeen { for_seq, accepted };
    if let Ok(req) = Request::post(&format!("/api/v1/sessions/{id}/suggestion/seen")).json(&body) {
        let _ = empty(req.send().await).await;
    }
}

/// A session's open decisions (not its approvals and questions: those are in its log).
pub async fn session_decisions(id: String) -> Result<Vec<c6_api::Decision>, String> {
    json(Request::get(&format!("/api/v1/sessions/{id}/decisions")).send().await).await
}

pub async fn decision_count() -> Result<i64, String> {
    let v: serde_json::Value = json(Request::get("/api/v1/decisions/count").send().await).await?;
    Ok(v["open"].as_i64().unwrap_or(0))
}

pub async fn edit_decision(id: i64, body: serde_json::Value) -> Result<c6_api::Decision, String> {
    send_json(Request::patch(&format!("/api/v1/decisions/{id}")), &body).await
}

pub async fn answer_decision(id: i64, a: c6_api::DecisionAnswer) -> Result<c6_api::Decision, String> {
    let d = send_json(Request::post(&format!("/api/v1/decisions/{id}/answer")), &a).await;
    // An app you approve may have made a realm of its own.
    bump_realms();
    d
}

pub async fn decision_session(id: i64) -> Result<c6_api::Session, String> {
    send_json(Request::post(&format!("/api/v1/decisions/{id}/session")), &serde_json::json!({})).await
}

// --- Collections (docs/os.md §4.1) ---------------------------------------------

pub async fn collections() -> Result<Vec<c6_api::Collection>, String> {
    json(Request::get(&format!("/api/v1/collections{}", in_realm('?'))).send().await).await
}

/// The views of the realm you are looking at (home's when "All realms").
pub async fn views() -> Result<Vec<c6_api::View>, String> {
    json(Request::get(&format!("/api/v1/views{}", in_realm('?'))).send().await).await
}

pub async fn view_act(name: String, a: c6_api::ViewAct) -> Result<serde_json::Value, String> {
    send_json(Request::post(&format!("/api/v1/views/{}/act{}", enc(&name), in_realm('?'))), &a).await
}

/// The items a messages, calendar or gallery view shows; `q` is more of
/// the query (`before=…`, `from=…&to=…`, `kind=video`).
pub async fn view_items(name: String, q: String) -> Result<Vec<c6_api::ViewItem>, String> {
    let realm = in_realm('&');
    json(Request::get(&format!("/api/v1/views/{}/items?{q}{realm}", enc(&name))).send().await).await
}

/// An item's picture in a view.
pub fn view_thumb(name: &str, id: &str) -> String {
    format!("/api/v1/views/{}/items/{}/thumb{}", enc(name), enc(id), in_realm('?'))
}

pub async fn series() -> Result<Vec<c6_api::Series>, String> {
    json(Request::get(&format!("/api/v1/series{}", in_realm('?'))).send().await).await
}

pub async fn create_series(s: c6_api::SeriesIn) -> Result<c6_api::Series, String> {
    send_json(Request::post(&format!("/api/v1/series{}", in_realm('?'))), &s).await
}

pub async fn delete_series(name: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/series/{}{}", enc(&name), in_realm('?'))).send().await).await
}

pub async fn query_series(name: String, q: c6_api::SeriesQuery) -> Result<c6_api::SeriesResult, String> {
    send_json(Request::post(&format!("/api/v1/series/{}/query{}", enc(&name), in_realm('?'))), &q).await
}

pub async fn collection(name: String) -> Result<c6_api::Collection, String> {
    json(Request::get(&format!("/api/v1/collections/{}{}", enc(&name), in_realm('?'))).send().await).await
}

/// A collection's data card (docs/apps.md §8).
pub async fn set_card(name: String, card: c6_api::DataCard) -> Result<c6_api::Collection, String> {
    let c = c6_api::CollectionIn { card: Some(card), ..Default::default() };
    send_json(Request::patch(&format!("/api/v1/collections/{}{}", enc(&name), in_realm('?'))), &c).await
}

pub async fn create_collection(c: c6_api::CollectionIn) -> Result<c6_api::Collection, String> {
    send_json(Request::post(&format!("/api/v1/collections{}", in_realm('?'))), &c).await
}

pub async fn drop_collection(name: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/collections/{}{}", enc(&name), in_realm('?'))).send().await).await
}

pub async fn query_records(name: String, q: c6_api::RecordQuery) -> Result<c6_api::RecordPage, String> {
    send_json(Request::post(&format!("/api/v1/collections/{}/query{}", enc(&name), in_realm('?'))), &q).await
}

pub async fn save_record(name: String, id: Option<String>, r: c6_api::RecordIn) -> Result<c6_api::Record, String> {
    match id {
        Some(id) => send_json(Request::put(&format!("/api/v1/collections/{}/records/{}{}", enc(&name), enc(&id), in_realm('?'))), &r).await,
        None => send_json(Request::post(&format!("/api/v1/collections/{}/records{}", enc(&name), in_realm('?'))), &r).await,
    }
}

pub async fn delete_record(name: String, id: String) -> Result<(), String> {
    empty(Request::delete(&format!("/api/v1/collections/{}/records/{}{}", enc(&name), enc(&id), in_realm('?'))).send().await).await
}

pub async fn record_history(name: String, id: String) -> Result<Vec<c6_api::RecordChange>, String> {
    json(Request::get(&format!("/api/v1/collections/{}/records/{}/history{}", enc(&name), enc(&id), in_realm('?'))).send().await).await
}
