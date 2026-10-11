//! Google in the memex, against a stand-in Google: OAuth sign-in on the
//! head; Gmail import, people and organizations, the mail views, archiving,
//! sending with undo; contacts (one person by two addresses), candidates
//! written back; calendars with a recurring event and an exception, RSVP;
//! an agent sending mail only after your approval; automations on new mail.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::routing::{get, patch, post};
use axum::{Form, Json, Router};
use base64::Engine;
use c6_api::{Connection, Event, EventKind, Session};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

async fn eventually<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(40);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn node(head: &c6_head::Head, memex: bool, stop: &CancellationToken) -> (Arc<Worker>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(dir.path()).unwrap();
    let w = Worker::open(dir.path(), Identity::load(dir.path()).unwrap()).await.unwrap();
    if memex {
        let m = c6_memex::Memex::open(&dir.path().join("memex"), w.link.clone()).await.unwrap();
        m.spawn_jobs();
        w.set_extension(m);
    }
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();
    (w, dir)
}

const DAY: i64 = 86_400_000;

/// The stand-in Google's state.
#[derive(Default)]
struct G {
    /// id → (thread, labels, internal date, raw)
    messages: HashMap<String, (String, Vec<String>, i64, String)>,
    order: Vec<String>,
    /// Messages added after the import (history).
    added: Vec<String>,
    history_id: u64,
    modified: Vec<(String, Value)>,
    sent: Vec<String>,
    patched: Vec<Value>,
    contacts_created: Vec<Value>,
    groups: Vec<String>,
    tokens: Vec<String>,
}

type Mock = Arc<Mutex<G>>;

fn raw(from: &str, to: &str, subject: &str, body: &str, extra: &str) -> String {
    let msg = format!(
        "From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\nMessage-ID: <{}@mail>\r\n{extra}Content-Type: text/plain\r\n\r\n{body}\r\n",
        subject.replace(' ', "-")
    );
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(msg)
}

fn add(g: &mut G, id: &str, thread: &str, labels: &[&str], date: i64, raw: String) {
    g.messages.insert(id.into(), (thread.into(), labels.iter().map(|l| l.to_string()).collect(), date, raw));
    g.order.insert(0, id.into());
}

async fn google(g: Mock) -> String {
    let app = Router::new()
        .route("/token", post(|State(g): State<Mock>, Form(f): Form<HashMap<String, String>>| async move {
            g.lock().unwrap().tokens.push(f.get("grant_type").cloned().unwrap_or_default());
            if f.get("grant_type").map(String::as_str) == Some("authorization_code") {
                assert_eq!(f.get("code").map(String::as_str), Some("the-code"));
                Json(json!({"access_token": "at-1", "refresh_token": "rt-1", "expires_in": 3600, "scope": "all"}))
            } else {
                Json(json!({"access_token": "at-2", "expires_in": 3600}))
            }
        }))
        .route("/oauth2/v3/userinfo", get(|| async { Json(json!({"email": "me@example.com", "name": "Me Myself"})) }))
        .route("/gmail/v1/users/me/profile", get(|State(g): State<Mock>| async move { Json(json!({"historyId": g.lock().unwrap().history_id.to_string()})) }))
        .route("/gmail/v1/users/me/labels", get(|| async { Json(json!({"labels": [{"id": "INBOX", "name": "INBOX", "type": "system"}, {"id": "Label_1", "name": "Acme", "type": "user"}]})) }))
        .route("/gmail/v1/users/me/messages", get(|State(g): State<Mock>, Query(q): Query<HashMap<String, String>>| async move {
            let g = g.lock().unwrap();
            let ids: Vec<&String> = g.order.iter().filter(|i| !g.added.contains(i)).collect();
            // Two pages.
            let (page, next) = if q.contains_key("pageToken") { (&ids[2.min(ids.len())..], None) } else { (&ids[..2.min(ids.len())], Some("p2")) };
            Json(json!({"messages": page.iter().map(|i| json!({"id": i})).collect::<Vec<_>>(), "nextPageToken": next}))
        }))
        .route("/gmail/v1/users/me/messages/{id}", get(|State(g): State<Mock>, Path(id): Path<String>| async move {
            let g = g.lock().unwrap();
            let (thread, labels, date, raw) = g.messages.get(&id).cloned().unwrap();
            Json(json!({"id": id, "threadId": thread, "labelIds": labels, "internalDate": date.to_string(), "raw": raw}))
        }))
        .route("/gmail/v1/users/me/history", get(|State(g): State<Mock>| async move {
            let g = g.lock().unwrap();
            let added: Vec<Value> = g.added.iter().map(|id| {
                let (thread, labels, _, _) = &g.messages[id];
                json!({"messagesAdded": [{"message": {"id": id, "threadId": thread, "labelIds": labels}}]})
            }).collect();
            Json(json!({"history": added, "historyId": g.history_id.to_string()}))
        }))
        .route("/gmail/v1/users/me/threads/{id}/modify", post(|State(g): State<Mock>, Path(id): Path<String>, Json(b): Json<Value>| async move {
            g.lock().unwrap().modified.push((id, b));
            Json(json!({}))
        }))
        .route("/gmail/v1/users/me/drafts", post(|| async { Json(json!({"id": "draft-1"})) }))
        .route("/gmail/v1/users/me/messages/send", post(|State(g): State<Mock>, Json(b): Json<Value>| async move {
            let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(b["raw"].as_str().unwrap().trim_end_matches('=')).unwrap();
            g.lock().unwrap().sent.push(String::from_utf8_lossy(&raw).into_owned());
            Json(json!({"id": "sent-1"}))
        }))
        .route("/calendar/v3/users/me/calendarList", get(|| async {
            Json(json!({"items": [{"id": "me@example.com", "summary": "Me", "primary": true, "accessRole": "owner", "timeZone": "UTC", "backgroundColor": "#9fe1e7"}]}))
        }))
        .route("/calendar/v3/calendars/{cal}/events", get(|Query(q): Query<HashMap<String, String>>| async move {
            if q.contains_key("syncToken") {
                return Json(json!({"items": [], "nextSyncToken": "s2"}));
            }
            Json(json!({"items": [
                {"id": "single", "status": "confirmed", "summary": "Acme review", "start": {"dateTime": "2026-11-03T15:00:00Z"}, "end": {"dateTime": "2026-11-03T16:00:00Z"},
                 "attendees": [{"email": "me@example.com", "self": true, "responseStatus": "needsAction"}, {"email": "alice@acme.com", "displayName": "Alice Smith"}]},
                {"id": "weekly", "status": "confirmed", "summary": "Standup", "start": {"dateTime": "2026-11-02T09:00:00Z", "timeZone": "UTC"}, "end": {"dateTime": "2026-11-02T09:15:00Z", "timeZone": "UTC"},
                 "recurrence": ["RRULE:FREQ=WEEKLY;COUNT=3"]},
                {"id": "weekly_20261109T090000Z", "status": "cancelled", "recurringEventId": "weekly", "originalStartTime": {"dateTime": "2026-11-09T09:00:00Z"}}
            ], "nextSyncToken": "s1"}))
        }).post(|Path(_cal): Path<String>, Json(mut b): Json<Value>| async move {
            b["id"] = json!("created-1");
            b["status"] = json!("confirmed");
            Json(b)
        }))
        .route("/calendar/v3/calendars/{cal}/events/{id}", patch(|State(g): State<Mock>, Path((_cal, id)): Path<(String, String)>, Json(b): Json<Value>| async move {
            g.lock().unwrap().patched.push(json!({"id": id, "body": b.clone()}));
            Json(json!({"id": id, "status": "confirmed", "summary": "Acme review", "start": {"dateTime": "2026-11-03T15:00:00Z"}, "end": {"dateTime": "2026-11-03T16:00:00Z"}, "attendees": b["attendees"]}))
        }))
        .route("/v1/people/me/connections", get(|| async {
            Json(json!({"connections": [{"resourceName": "people/c1", "etag": "e1", "names": [{"displayName": "Alice Smith"}],
                "emailAddresses": [{"value": "alice@acme.com"}, {"value": "alice.smith@gmail.com"}], "organizations": [{"name": "Acme"}]}], "nextSyncToken": "p1"}))
        }))
        .route("/v1/contactGroups", get(|State(g): State<Mock>| async move {
            let groups: Vec<Value> = g.lock().unwrap().groups.iter().map(|n| json!({"name": n, "resourceName": format!("contactGroups/{n}")})).collect();
            Json(json!({"contactGroups": groups}))
        }).post(|State(g): State<Mock>, Json(b): Json<Value>| async move {
            let name = b["contactGroup"]["name"].as_str().unwrap().to_owned();
            g.lock().unwrap().groups.push(name.clone());
            Json(json!({"name": name, "resourceName": format!("contactGroups/{name}")}))
        }))
        .route("/v1/people:createContact", post(|State(g): State<Mock>, Json(b): Json<Value>| async move {
            g.lock().unwrap().contacts_created.push(b.clone());
            Json(json!({"resourceName": "people/c2", "names": [{"displayName": b["names"][0]["unstructuredName"]}], "emailAddresses": b["emailAddresses"], "memberships": b["memberships"]}))
        }))
        .with_state(g);
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    url
}

#[tokio::test]
async fn google_mail_calendar_contacts() {
    let now = c6_core::now_ms();
    let g: Mock = Arc::default();
    {
        let mut s = g.lock().unwrap();
        s.history_id = 100;
        add(
            &mut s,
            "m1",
            "t1",
            &["INBOX", "UNREAD"],
            now - DAY,
            raw("Alice Smith <alice@acme.com>", "Me <me@example.com>", "Lunch on Friday?", "Are you free for lunch on Friday?", ""),
        );
        add(
            &mut s,
            "m2",
            "t2",
            &["INBOX"],
            now - 2 * DAY,
            raw("News <news@lists.example.org>", "me@example.com", "Weekly digest", "Top stories", "List-Unsubscribe: <mailto:x>\r\n"),
        );
        add(
            &mut s,
            "m3",
            "t3",
            &["SENT"],
            now - 5 * DAY,
            raw("Me <me@example.com>", "Bob <bob@other.org>", "Proposal", "Here is the proposal.", ""),
        );
        add(
            &mut s,
            "m4",
            "t3",
            &["INBOX"],
            now - 4 * DAY,
            raw("Bob <bob@other.org>", "me@example.com", "Re: Proposal", "Thanks, will read.", ""),
        );
        add(&mut s, "m5", "t4", &["SENT"], now - 3 * DAY, raw("Me <me@example.com>", "bob@other.org", "Follow-up", "Any thoughts?", ""));
    }
    let base = google(g.clone()).await;
    // SAFETY: the only test in this binary, before anything reads them.
    unsafe {
        std::env::set_var("C6_GOOGLE_AUTH_URL", format!("{base}/auth"));
        std::env::set_var("C6_GOOGLE_TOKEN_URL", format!("{base}/token"));
        std::env::set_var("C6_GOOGLE_API_URL", &base);
        std::env::set_var("C6_GOOGLE_PEOPLE_URL", &base);
    }
    let mock = MockLlm::start().await;
    let head_dir = tempfile::tempdir().unwrap();
    let head = c6_head::start(HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: false,
        ..Default::default()
    })
    .await
    .unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let http = reqwest::Client::new();
    let stop = CancellationToken::new();

    // Sign-in.
    assert_eq!(
        http.post(format!("{api}/google/connect")).send().await.unwrap().status(),
        reqwest::StatusCode::BAD_REQUEST,
        "no client yet"
    );
    assert!(
        http.put(format!("{api}/google/client"))
            .json(&json!({"client_id": "cid", "client_secret": "csecret"}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let c: Value = http.get(format!("{api}/google/client")).send().await.unwrap().json().await.unwrap();
    assert!(c["has_secret"].as_bool().unwrap() && c["redirect_uri"].as_str().unwrap().ends_with("/auth/google/callback"), "{c}");
    let go: Value = http.post(format!("{api}/google/connect")).send().await.unwrap().json().await.unwrap();
    let url = url::Url::parse(go["authorize_url"].as_str().unwrap()).unwrap();
    let q: HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!((q["client_id"].as_str(), q["access_type"].as_str()), ("cid", "offline"));
    assert!(q["scope"].contains("gmail.modify") && q["scope"].contains("calendar") && q["scope"].contains("contacts"));
    let ui = api.trim_end_matches("/api/v1");
    let back = http.get(format!("{ui}/auth/google/callback?code=the-code&state={}", q["state"])).send().await.unwrap();
    assert!(back.url().as_str().contains("google=connected"), "{}", back.url());
    let replay = http.get(format!("{ui}/auth/google/callback?code=the-code&state={}", q["state"])).send().await.unwrap();
    assert!(replay.url().as_str().contains("expired"), "a state works once");
    let accounts: Vec<c6_api::GoogleAccount> = http.get(format!("{api}/google/accounts")).send().await.unwrap().json().await.unwrap();
    assert_eq!(accounts[0].email, "me@example.com");

    let put = |path: &str, v: Value| {
        let (http, url) = (http.clone(), format!("{api}/settings/{path}"));
        async move { http.put(url).json(&v).send().await.unwrap().status() }
    };
    assert!(
        put("memex_scopes", json!([{"name": "Mail sessions", "applies_to": ["mail"], "kinds": ["email", "event"]}])).await.is_success()
    );
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let (worker, _wd) = node(&head, false, &stop).await;
    let (_memex, _md) = node(&head, true, &stop).await;
    let rpc = |method: &str, params: Value| {
        let (http, url, body) = (http.clone(), format!("{api}/memex/rpc"), json!({"method": method, "params": params}));
        async move {
            let r = http.post(url).json(&body).send().await.unwrap();
            assert!(r.status().is_success(), "{}", r.text().await.unwrap());
            r.json::<Value>().await.unwrap()
        }
    };

    // Imported (both pages), contacts synced, calendars read.
    eventually("the import", async || {
        let v = rpc("memex.google", json!({})).await;
        let phase = |k: &str| v["sync"].as_array()?.iter().find(|s| s["kind"] == k).map(|s| s["phase"].as_str().unwrap_or("").to_owned());
        (phase("mail")? == "live" && phase("contacts")? == "live" && phase("cal:me@example.com")? == "live").then_some(())
    })
    .await;
    let inbox = rpc("memex.mail.threads", json!({"view": "inbox"})).await;
    assert_eq!(inbox.as_array().unwrap().len(), 3, "{inbox}");
    let needs = rpc("memex.mail.threads", json!({"view": "needs_reply"})).await;
    let subjects: Vec<&str> = needs.as_array().unwrap().iter().map(|t| t["subject"].as_str().unwrap()).collect();
    assert_eq!(subjects, ["Lunch on Friday?", "Re: Proposal"], "not the list mail");
    let waiting = rpc("memex.mail.threads", json!({"view": "waiting"})).await;
    assert_eq!(waiting[0]["subject"], "Follow-up", "{waiting}");
    let thread = rpc("memex.mail.thread", json!({"account": "me@example.com", "thread_id": "t3"})).await;
    assert_eq!(thread.as_array().unwrap().len(), 2);
    assert!(thread[1]["text"].as_str().unwrap().contains("will read"));

    // People: one person by both addresses (from the contact), and an
    // organization for the company domain.
    let get = |id: &str| {
        let (http, url) = (http.clone(), format!("{api}/memex/get?id={id}"));
        async move { http.get(url).send().await.unwrap().json::<Value>().await.unwrap() }
    };
    let alice = get("person:alice@acme.com").await;
    let alias = get("person:alice.smith@gmail.com").await;
    assert_eq!(alice["thing"]["thing"]["id"], alias["thing"]["thing"]["id"]);
    assert_eq!(alice["thing"]["thing"]["name"], "Alice Smith");
    assert!(alice["thing"]["items"].to_string().contains("Lunch on Friday?"), "{alice}");
    assert_eq!(get("org:acme.com").await["thing"]["thing"]["kind"], "org");

    // Archive a thread: Gmail is told, the view follows.
    rpc("memex.mail.modify", json!({"account": "me@example.com", "thread_id": "t2", "remove": ["INBOX"]})).await;
    assert_eq!(g.lock().unwrap().modified[0].0, "t2");
    assert_eq!(rpc("memex.mail.threads", json!({"view": "inbox"})).await.as_array().unwrap().len(), 2);

    // Sending: undone within the window; or sent after it.
    let o = rpc("memex.mail.send", json!({"account": "me@example.com", "to": ["bob@other.org"], "subject": "Oops", "body": "never mind"}))
        .await;
    assert_eq!(rpc("memex.mail.cancel", json!({"id": o["id"]})).await["cancelled"], true);
    rpc(
        "memex.mail.send",
        json!({"account": "me@example.com", "to": ["alice@acme.com"], "subject": "Re: Lunch on Friday?", "body": "**Yes**, Friday works.",
                                  "reply_to": thread[0]["item_id"]}),
    )
    .await;
    let sent = eventually("the message sent", async || g.lock().unwrap().sent.first().cloned()).await;
    assert!(
        sent.contains("Subject: Re: Lunch on Friday?") && sent.contains("<strong>Yes</strong>") && !sent.contains("never mind"),
        "{sent}"
    );

    // Contacts: Bob (written to twice, heard from) is a candidate; adding
    // him writes to Google in the carbon6 group.
    let cands = rpc("memex.contacts.candidates", json!({})).await;
    assert_eq!((cands[0]["email"].as_str(), cands[0]["sent"].as_u64()), (Some("bob@other.org"), Some(2)), "{cands}");
    rpc("memex.contacts.create", json!({"account": "me@example.com", "name": "Bob", "email": "bob@other.org"})).await;
    let created = g.lock().unwrap().contacts_created[0].clone();
    assert_eq!(created["memberships"][0]["contactGroupMembership"]["contactGroupResourceName"], "contactGroups/carbon6");
    assert!(rpc("memex.contacts.candidates", json!({})).await.as_array().unwrap().iter().all(|c| c["email"] != "bob@other.org"));

    // Calendar: the weekly standup three times minus a cancelled one, and
    // the review; answering the invitation.
    let from = chrono_ms("2026-11-01T00:00:00Z");
    let events = rpc("memex.calendar.events", json!({"from": from, "to": from + 30 * DAY})).await;
    let titles: Vec<&str> = events.as_array().unwrap().iter().map(|e| e["summary"].as_str().unwrap()).collect();
    assert_eq!(titles, ["Standup", "Acme review", "Standup"], "{events}");
    rpc(
        "memex.calendar.rsvp",
        json!({"account": "me@example.com", "calendar_id": "me@example.com", "event_id": "single", "response": "accepted"}),
    )
    .await;
    let p = g.lock().unwrap().patched[0].clone();
    assert_eq!(p["body"]["attendees"][0]["responseStatus"], "accepted", "{p}");

    // An agent in a mail session: mail tools; sending waits for you.
    let wid = worker.identity.meta.worker_id.0;
    mock.push(Reply::ToolCalls(vec![("mail_threads".into(), r#"{"view": "needs_reply"}"#.into())]));
    mock.push(Reply::ToolCalls(vec![(
        "mail_send".into(),
        r#"{"to": ["bob@other.org"], "subject": "Re: Proposal", "body": "Glad to hear it."}"#.into(),
    )]));
    mock.push(Reply::Text("Replied.".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "answer what needs answering", "labels": ["mail"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let call = eventually("the approval request", async || {
        let evs: Vec<Event> = http.get(format!("{api}/sessions/{}/events", s.id)).send().await.ok()?.json().await.ok()?;
        evs.iter().find_map(|e| match &e.kind {
            EventKind::Approval { call_id, state, tool, .. } if state == "requested" && tool == "mail_send" => Some(call_id.clone()),
            _ => None,
        })
    })
    .await;
    let listed = mock.requests()[1]["messages"].to_string();
    assert!(listed.contains("Lunch on Friday?") && listed.contains("thread t1"), "{listed}");
    assert_eq!(g.lock().unwrap().sent.len(), 1, "nothing sent before you say so");
    http.post(format!("{api}/sessions/{}/approvals/{call}", s.id)).json(&json!({"allow": true})).send().await.unwrap();
    eventually("the agent's mail sent", async || (g.lock().unwrap().sent.len() == 2).then_some(())).await;
    assert!(g.lock().unwrap().sent[1].contains("Glad to hear it."));
    // A session without mail in its scope has no mail tools.
    let before = mock.requests().len();
    mock.push(Reply::Text("ok".into()));
    http.post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "hi"}))
        .send()
        .await
        .unwrap();
    eventually("a plain session's call", async || (mock.requests().len() > before).then_some(())).await;
    let tools = mock.requests()[before]["tools"].to_string();
    assert!(tools.contains("memex_search") && !tools.contains("mail_send"), "{tools}");

    // Automations on new mail.
    assert!(
        put("templates", json!([{"name": "triage", "prompt": "Triage: {{input}}", "connection": "m", "model": "m"}])).await.is_success()
    );
    let bad = http
        .post(format!("{api}/automations"))
        .json(&json!({"name": "x", "template": "triage", "trigger": "memex", "cron": "mail.sent"}))
        .send()
        .await
        .unwrap();
    assert_eq!(bad.status(), reqwest::StatusCode::BAD_REQUEST);
    let a: c6_api::Automation = http
        .post(format!("{api}/automations"))
        .json(&json!({"name": "acme mail", "template": "triage", "trigger": "memex", "cron": "mail.received from:@acme.com"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let before = mock.requests().len();
    mock.push(Reply::Text("Triaged.".into()));
    {
        let mut s = g.lock().unwrap();
        add(
            &mut s,
            "m6",
            "t6",
            &["INBOX", "UNREAD"],
            now,
            raw("Alice Smith <alice@acme.com>", "me@example.com", "Contract draft", "Please review the attached contract.", ""),
        );
        add(&mut s, "m7", "t7", &["INBOX"], now, raw("Carol <carol@else.com>", "me@example.com", "Hello", "Hi there", ""));
        s.added = vec!["m6".into(), "m7".into()];
        s.history_id = 200;
    }
    rpc("memex.google_sync", json!({})).await;
    let runs = eventually("the automation's run", async || {
        let r: Vec<c6_api::AutomationRun> = http.get(format!("{api}/automations/{}/runs", a.id)).send().await.ok()?.json().await.ok()?;
        (!r.is_empty()).then_some(r)
    })
    .await;
    assert_eq!((runs.len(), runs[0].trigger.as_str()), (1, "memex"), "only Acme's mail");
    eventually("the triage session", async || (mock.requests().len() > before).then_some(())).await;
    let asked = mock.requests()[before]["messages"].to_string();
    assert!(asked.contains("Triage: New email from Alice Smith <alice@acme.com>") && asked.contains("Contract draft"), "{asked}");
    assert!(g.lock().unwrap().tokens.contains(&"authorization_code".to_owned()));

    stop.cancel();
    head.shutdown().await.unwrap();
}

fn chrono_ms(s: &str) -> i64 {
    // 2026-11-01T00:00:00Z
    let days_from_epoch = {
        let (y, m, d) = (s[0..4].parse::<i64>().unwrap(), s[5..7].parse::<i64>().unwrap(), s[8..10].parse::<i64>().unwrap());
        let (y, m) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * m + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    };
    days_from_epoch * DAY
}
