//! Notifications: the head tells you about events in your sessions
//! (approvals, failures, the agent's notices, …) on your channels: ntfy,
//! Slack, Discord, a webhook or email. Approval notifications carry a
//! signed link to a page with Allow / Deny that works without signing in.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Form;
use axum::extract::{Path, State};
use axum::response::Html;
use c6_api as api;
use ring::hmac;
use serde_json::json;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::{HeadState, bad_request};

/// Settings keys.
pub const CHANNELS: &str = "channels";
pub const PUBLIC_URL: &str = "public_url";
const LINK_KEY: &str = "link_key";

/// How long an approval link works (a held call waits up to an hour).
const LINK_TTL_MS: i64 = 60 * 60 * 1000;

/// Something to tell you.
#[derive(Clone, Debug, Default)]
pub struct Note {
    /// One of `api::NOTIFY_EVENTS`.
    pub event: &'static str,
    pub title: String,
    pub message: String,
    /// "low", "normal" or "high".
    pub priority: &'static str,
    pub session_id: Option<String>,
    /// The session's labels (or the automation's), for channels' `applies_to`.
    pub labels: Vec<String>,
    /// For "approval": the held call.
    pub call_id: Option<String>,
}

/// The queue to the sender task.
pub struct Notifier {
    tx: mpsc::UnboundedSender<Note>,
    rx: Mutex<Option<mpsc::UnboundedReceiver<Note>>>,
}

impl Default for Notifier {
    fn default() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self { tx, rx: Mutex::new(Some(rx)) }
    }
}

impl Notifier {
    pub fn send(&self, note: Note) {
        let _ = self.tx.send(note);
    }
}

/// Sends queued notes until shutdown, one at a time (in order).
pub fn spawn(st: Arc<HeadState>) {
    let Some(mut rx) = st.notifier.rx.lock().unwrap().take() else { return };
    tokio::spawn(async move {
        loop {
            let note = tokio::select! {
                n = rx.recv() => match n { Some(n) => n, None => return },
                _ = st.shutdown.cancelled() => return,
            };
            let channels = match channels(&st).await {
                Ok(c) => c,
                Err(e) => {
                    warn!("loading notification channels: {e:#}");
                    continue;
                }
            };
            let wanted: Vec<&api::Channel> = channels.iter().filter(|c| wants(c, &note)).collect();
            if wanted.is_empty() {
                continue;
            }
            let links = match links(&st, &note).await {
                Ok(l) => l,
                Err(e) => {
                    warn!("making notification links: {e:#}");
                    continue;
                }
            };
            for ch in wanted {
                if let Err(e) = deliver(&st, ch, &note, &links).await {
                    warn!(channel = %ch.name, event = note.event, "notification not sent: {e:#}");
                }
            }
        }
    });
}

pub async fn channels(st: &HeadState) -> anyhow::Result<Vec<api::Channel>> {
    Ok(serde_json::from_str(&st.store.setting(CHANNELS).await?.unwrap_or_default()).unwrap_or_default())
}

/// Whether a channel carries a note.
fn wants(ch: &api::Channel, note: &Note) -> bool {
    let events: Vec<&str> = if ch.events.iter().all(|e| e.trim().is_empty()) {
        api::DEFAULT_NOTIFY_EVENTS.to_vec()
    } else {
        ch.events.iter().map(|e| e.trim()).collect()
    };
    ch.enabled && events.contains(&note.event) && api::labels::applies(&ch.applies_to, &note.labels)
}

/// Checks channels before they are saved.
pub fn check_channels(channels: &[api::Channel]) -> anyhow::Result<()> {
    for c in channels {
        let what = |m: &str| bad_request(format!("channel {:?}: {m}", c.name));
        if c.name.trim().is_empty() {
            return Err(bad_request("a channel needs a name"));
        }
        match c.kind.as_str() {
            "ntfy" | "slack" | "discord" | "webhook" => {
                let u = url::Url::parse(c.url.trim()).map_err(|_| what("the URL is not valid"))?;
                if !matches!(u.scheme(), "http" | "https") {
                    return Err(what("the URL is http(s)://…"));
                }
                if c.kind == "ntfy" && u.path_segments().and_then(|mut s| s.next_back()).is_none_or(str::is_empty) {
                    return Err(what("the URL names a topic, like https://ntfy.sh/my-topic"));
                }
            }
            "email" => {
                if c.url.trim().is_empty() || c.url.contains("://") {
                    return Err(what("the server is host[:port], like smtp.gmail.com:587"));
                }
                if !c.user.contains('@') || !c.to.contains('@') {
                    return Err(what("email needs a user (the sender's address) and a recipient"));
                }
            }
            _ => return Err(what("the kind is ntfy, slack, discord, webhook or email")),
        }
        for e in c.events.iter().map(|e| e.trim()).filter(|e| !e.is_empty()) {
            if !api::NOTIFY_EVENTS.iter().any(|(n, _)| *n == e) {
                let names: Vec<&str> = api::NOTIFY_EVENTS.iter().map(|(n, _)| *n).collect();
                return Err(what(&format!("{e:?} is not an event ({})", names.join(", "))));
            }
        }
        crate::sessions::check_selector(&c.applies_to)?;
    }
    Ok(())
}

/// Sends a test message on a channel, now.
pub async fn test(st: &HeadState, name: &str) -> anyhow::Result<()> {
    let channels = channels(st).await?;
    let ch = channels.iter().find(|c| c.name == name).ok_or_else(|| bad_request("no such channel"))?;
    let note = Note {
        event: "notice",
        title: "carbon6 test".into(),
        message: "Notifications from carbon6 arrive here.".into(),
        priority: "normal",
        ..Default::default()
    };
    deliver(st, ch, &note, &Links::default()).await.map_err(|e| bad_request(format!("{e:#}")))
}

/// The address links point to: the setting, without a trailing slash.
async fn public_url(st: &HeadState) -> String {
    st.store.setting(PUBLIC_URL).await.ok().flatten().unwrap_or_default().trim().trim_end_matches('/').to_owned()
}

async fn secret(st: &HeadState, name: &str) -> anyhow::Result<Option<String>> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(None);
    }
    let (blob, _) = st.store.secret(name).await?.ok_or_else(|| anyhow::anyhow!("no secret named {name}"))?;
    Ok(Some(st.secrets.decrypt(&blob)?))
}

/// A note's links (one approval link for every channel).
#[derive(Default)]
struct Links {
    session_url: Option<String>,
    approve_url: Option<String>,
}

async fn links(st: &HeadState, note: &Note) -> anyhow::Result<Links> {
    let base = public_url(st).await;
    if base.is_empty() {
        return Ok(Links::default());
    }
    let session_url = note.session_id.as_ref().map(|id| format!("{base}/sessions/{id}"));
    let approve_url = match (&note.session_id, &note.call_id) {
        (Some(s), Some(c)) => Some(format!("{base}/approve/{}", sign(st, s, c, c6_core::now_ms() + LINK_TTL_MS).await?)),
        _ => None,
    };
    Ok(Links { session_url, approve_url })
}

async fn deliver(st: &HeadState, ch: &api::Channel, note: &Note, links: &Links) -> anyhow::Result<()> {
    let Links { session_url, approve_url } = links;
    let token = secret(st, &ch.secret).await?;
    let http = &st.http;
    let mut text = note.message.clone();
    let send = |req: reqwest::RequestBuilder| async move {
        let r = req.timeout(Duration::from_secs(15)).send().await?;
        let status = r.status();
        if !status.is_success() {
            let body = r.text().await.unwrap_or_default();
            anyhow::bail!("{status}: {}", body.chars().take(300).collect::<String>());
        }
        anyhow::Ok(())
    };
    match ch.kind.as_str() {
        "ntfy" => {
            // JSON publishing (to the server root) carries Unicode titles.
            let u = url::Url::parse(ch.url.trim())?;
            let topic = u.path_segments().and_then(|mut s| s.next_back()).unwrap_or_default().to_owned();
            let mut root = u.clone();
            root.set_path("/");
            let priority = match note.priority {
                "low" => 2,
                "high" => 5,
                _ => 3,
            };
            let mut actions = Vec::new();
            if let Some(a) = &approve_url {
                actions.push(json!({"action": "view", "label": "Allow or deny", "url": a, "clear": true}));
            }
            if let Some(s) = &session_url {
                actions.push(json!({"action": "view", "label": "Open session", "url": s}));
            }
            let mut body = json!({"topic": topic, "title": note.title, "message": if text.is_empty() { note.title.clone() } else { text },
                                  "priority": priority, "tags": [note.event], "actions": actions});
            if let Some(s) = &session_url {
                body["click"] = json!(s);
            }
            let mut req = http.post(root).json(&body);
            if let Some(t) = token {
                req = req.bearer_auth(t);
            }
            send(req).await
        }
        "slack" | "discord" => {
            for (label, u) in [("Allow or deny", &approve_url), ("Open session", &session_url)] {
                if let Some(u) = u {
                    text.push_str(&if ch.kind == "slack" { format!("\n<{u}|{label}>") } else { format!("\n{label}: {u}") });
                }
            }
            let body = if ch.kind == "slack" {
                json!({"text": format!("*{}*\n{}", note.title, text.trim_start())})
            } else {
                json!({"content": format!("**{}**\n{}", note.title, text.trim_start())})
            };
            send(http.post(ch.url.trim()).json(&body)).await
        }
        "webhook" => {
            let body = json!({"event": note.event, "title": note.title, "message": note.message, "priority": note.priority,
                              "session_id": note.session_id, "session_url": session_url, "approve_url": approve_url,
                              "labels": note.labels});
            let mut req = http.post(ch.url.trim()).json(&body);
            if let Some(t) = token {
                req = req.bearer_auth(t);
            }
            send(req).await
        }
        "email" => {
            for (label, u) in [("Allow or deny", &approve_url), ("Open the session", &session_url)] {
                if let Some(u) = u {
                    text.push_str(&format!("\n\n{label}: {u}"));
                }
            }
            email(ch, token, &note.title, text.trim_start()).await
        }
        k => anyhow::bail!("unknown channel kind {k:?}"),
    }
}

async fn email(ch: &api::Channel, password: Option<String>, subject: &str, body: &str) -> anyhow::Result<()> {
    use lettre::message::header::ContentType;
    use lettre::transport::smtp::authentication::Credentials;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};

    let (host, port) = match ch.url.trim().rsplit_once(':') {
        Some((h, p)) => (h.to_owned(), p.parse::<u16>().map_err(|_| anyhow::anyhow!("bad port in {}", ch.url))?),
        None => (ch.url.trim().to_owned(), 587),
    };
    let msg = Message::builder()
        .from(format!("carbon6 <{}>", ch.user.trim()).parse()?)
        .to(ch.to.trim().parse()?)
        .subject(format!("[carbon6] {subject}"))
        .header(ContentType::TEXT_PLAIN)
        .body(body.to_owned())?;
    let builder = if port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&host)?
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&host)?
    };
    let mut builder = builder.port(port).timeout(Some(Duration::from_secs(20)));
    if let Some(p) = password {
        builder = builder.credentials(Credentials::new(ch.user.trim().to_owned(), p));
    }
    builder.build().send(msg).await?;
    Ok(())
}

// --- Signed approval links ---------------------------------------------------

async fn link_key(st: &HeadState) -> anyhow::Result<hmac::Key> {
    let hex_key = match st.store.setting(LINK_KEY).await? {
        Some(k) if !k.is_empty() => k,
        _ => {
            let k = hex::encode(rand::random::<[u8; 32]>());
            st.store.set_setting(LINK_KEY, &k).await?;
            k
        }
    };
    Ok(hmac::Key::new(hmac::HMAC_SHA256, &hex::decode(hex_key)?))
}

/// `<session>.<call>.<expiry ms>.<signature>`, URL-safe.
async fn sign(st: &HeadState, session_id: &str, call_id: &str, expires_ms: i64) -> anyhow::Result<String> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let payload = format!("{}.{}.{expires_ms}", b64.encode(session_id), b64.encode(call_id));
    let tag = hmac::sign(&link_key(st).await?, payload.as_bytes());
    Ok(format!("{payload}.{}", b64.encode(tag.as_ref())))
}

/// The session and call a link is for, if it is genuine and not expired.
async fn verify(st: &HeadState, token: &str) -> Option<(String, String)> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let (payload, tag) = token.rsplit_once('.')?;
    let key = link_key(st).await.ok()?;
    hmac::verify(&key, payload.as_bytes(), &b64.decode(tag).ok()?).ok()?;
    let mut parts = payload.split('.');
    let session = String::from_utf8(b64.decode(parts.next()?).ok()?).ok()?;
    let call = String::from_utf8(b64.decode(parts.next()?).ok()?).ok()?;
    let expires: i64 = parts.next()?.parse().ok()?;
    (c6_core::now_ms() < expires).then_some((session, call))
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn page(title: &str, body: &str) -> Html<String> {
    Html(format!(
        "<!doctype html><html><head><meta charset=utf-8><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <meta name=referrer content=same-origin><title>{t}</title><style>\
         body{{font:16px system-ui,sans-serif;max-width:40rem;margin:2rem auto;padding:0 1rem;color:#18181b;background:#fff}}\
         @media(prefers-color-scheme:dark){{body{{color:#f4f4f5;background:#09090b}}pre{{background:#18181b!important}}}}\
         pre{{background:#f4f4f5;padding:.75rem;border-radius:.5rem;white-space:pre-wrap;word-break:break-word;font-size:13px}}\
         button{{font:inherit;padding:.6rem 1.4rem;border-radius:.5rem;border:0;color:#fff;margin-right:.5rem}}\
         .allow{{background:#059669}}.deny{{background:#dc2626}}.muted{{color:#71717a;font-size:14px}}</style></head>\
         <body><h1 style=\"font-size:1.2rem\">{t}</h1>{body}</body></html>",
        t = esc(title)
    ))
}

/// What a held call is, from its approval request event.
async fn describe(st: &HeadState, session_id: &str, call_id: &str) -> Option<(String, String, String, String)> {
    let events = st.store.events_after(session_id, 0, i64::MAX).await.ok()?;
    events.iter().rev().find_map(|e| match &e.kind {
        Some(c6_proto::session_event::Kind::Approval(a)) if a.call_id == call_id && a.state == "requested" => {
            Some((a.tool.clone(), a.input.clone(), a.policy.clone(), a.message.clone()))
        }
        _ => None,
    })
}

/// Whether the call still waits: requested, and nothing logged since.
async fn still_waiting(st: &HeadState, session_id: &str, call_id: &str) -> bool {
    let Ok(events) = st.store.events_after(session_id, 0, i64::MAX).await else { return false };
    let mut waiting = false;
    for e in &events {
        if let Some(c6_proto::session_event::Kind::Approval(a)) = &e.kind
            && a.call_id == call_id
        {
            waiting = a.state == "requested";
        }
    }
    waiting
}

const INVALID: &str = "<p>This link is not valid or has expired. Open the session in carbon6 to answer.</p>";

/// `GET /approve/{token}`: what waits, with Allow / Deny (POSTs, so link
/// previews in chat apps cannot answer).
pub async fn approve_page(State(st): State<Arc<HeadState>>, Path(token): Path<String>) -> Html<String> {
    let Some((session_id, call_id)) = verify(&st, &token).await else { return page("Approval", INVALID) };
    let title = st.store.session(&session_id).await.ok().flatten().map(|s| s.title).unwrap_or_default();
    let Some((tool, input, policy, message)) = describe(&st, &session_id, &call_id).await else {
        return page("Approval", INVALID);
    };
    if !still_waiting(&st, &session_id, &call_id).await {
        return page("Approval", "<p>This call was already answered (or the wait ended).</p>");
    }
    let why = if message.is_empty() { String::new() } else { format!("<p>{}</p>", esc(&message)) };
    page(
        &format!("Approve {tool}?"),
        &format!(
            "<p class=muted>Session: {}<br>Policy: {}</p>{why}<pre>{}</pre>\
             <form method=post><button class=allow name=allow value=1>Allow</button><button class=deny name=allow value=0>Deny</button></form>",
            esc(&title),
            esc(&policy),
            esc(&input)
        ),
    )
}

#[derive(serde::Deserialize)]
pub struct Answer {
    allow: String,
}

/// `POST /approve/{token}`.
pub async fn approve_answer(State(st): State<Arc<HeadState>>, Path(token): Path<String>, Form(a): Form<Answer>) -> Html<String> {
    let Some((session_id, call_id)) = verify(&st, &token).await else { return page("Approval", INVALID) };
    if !still_waiting(&st, &session_id, &call_id).await {
        return page("Approval", "<p>This call was already answered (or the wait ended).</p>");
    }
    let allow = a.allow == "1";
    match crate::sessions::decide(&st, &session_id, &call_id, allow).await {
        Ok(true) => {
            info!(session = %session_id, call = %call_id, allow, "answered from a notification link");
            page(if allow { "Allowed" } else { "Denied" }, "<p>Sent. The session goes on.</p>")
        }
        Ok(false) => page("Not sent", "<p>The session's worker is not connected. Try again when it is back.</p>"),
        Err(e) => page("Not sent", &format!("<p>{}</p>", esc(&format!("{e:#}")))),
    }
}
