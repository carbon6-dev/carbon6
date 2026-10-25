//! Slack as an app (docs/slack.md), against a stand-in Slack, its kit's
//! code run in this process: installing it, the manifest and sign-in
//! address; the import across runs, with threads, DMs, people and a shared
//! document; a rate limit; keeping up; edits and deletions caught by the
//! re-read; posting as you; an agent's post only after your approval; and
//! another realm seeing none of it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use c6_api::{Connection, Event, EventKind, Session};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// The stand-in Slack's state.
#[derive(Default)]
struct S {
    base: String,
    /// Messages by conversation, and thread replies by (conversation, thread).
    messages: HashMap<String, Vec<Value>>,
    replies: HashMap<(String, String), Vec<Value>>,
    posted: Vec<Value>,
    reactions: Vec<Value>,
    marked: Vec<Value>,
}

type Mock = Arc<Mutex<S>>;

fn ts(seconds_ago: i64, n: u32) -> String {
    format!("{}.{n:06}", c6_core::now_ms() / 1000 - seconds_ago)
}

fn secs(ts: &str) -> f64 {
    ts.parse().unwrap_or(0.0)
}

/// Answers `ok: false` the way Slack does.
fn fail(e: &str) -> Json<Value> {
    Json(json!({"ok": false, "error": e}))
}

async fn slack(s: Mock, limited: Arc<AtomicBool>) -> String {
    let app = Router::new()
        .route("/api/team.info", get(|| async { Json(json!({"ok": true, "team": {"id": "T1", "name": "Acme", "domain": "acme"}})) }))
        .route(
            "/api/users.list",
            get(move || {
                let limited = limited.clone();
                async move {
                    // Once: slow down (with no wait, so the test needn't wait).
                    if limited.swap(false, Ordering::SeqCst) {
                        return axum::response::Response::builder()
                            .status(429)
                            .header("Retry-After", "0")
                            .body(axum::body::Body::empty())
                            .unwrap();
                    }
                    let members = json!([
                        {"id": "U1", "name": "ana", "profile": {"real_name": "Ana Lima", "email": "ana@acme.com"}},
                        {"id": "U2", "name": "bo", "profile": {"real_name": "Bo", "email": ""}},
                        {"id": "UME", "name": "me", "profile": {"real_name": "Me Myself", "email": "me@acme.com"}}
                    ]);
                    axum::response::IntoResponse::into_response(Json(
                        json!({"ok": true, "members": members, "response_metadata": {"next_cursor": ""}}),
                    ))
                }
            }),
        )
        .route(
            "/api/users.conversations",
            get(|| async {
                Json(json!({"ok": true, "channels": [
                    {"id": "C1", "name": "eng", "is_private": false, "updated": 3},
                    {"id": "D1", "is_im": true, "user": "U1", "updated": 2},
                    {"id": "C2", "name": "big", "is_private": false, "updated": 1}
                ], "response_metadata": {"next_cursor": ""}}))
            }),
        )
        .route(
            "/api/conversations.history",
            get(|State(s): State<Mock>, Query(q): Query<HashMap<String, String>>| async move {
                let s = s.lock().unwrap();
                let Some(all) = s.messages.get(&q["channel"]) else { return fail("channel_not_found") };
                let oldest = q.get("oldest").map_or(0.0, |t| secs(t));
                let latest = q.get("latest").map_or(f64::MAX, |t| secs(t));
                let mut list: Vec<Value> = all
                    .iter()
                    .filter(|m| secs(m["ts"].as_str().unwrap()) > oldest && secs(m["ts"].as_str().unwrap()) < latest)
                    .cloned()
                    .collect();
                list.sort_by(|a, b| secs(b["ts"].as_str().unwrap()).total_cmp(&secs(a["ts"].as_str().unwrap())));
                // Small pages, so an import takes many requests.
                let at: usize = q.get("cursor").and_then(|c| c.parse().ok()).unwrap_or(0);
                let page = if q["channel"] == "C2" { 2 } else { 200 };
                let end = (at + page).min(list.len());
                let more = end < list.len();
                Json(json!({"ok": true, "messages": list[at.min(end)..end], "has_more": more,
                            "response_metadata": {"next_cursor": if more { end.to_string() } else { String::new() }}}))
            }),
        )
        .route(
            "/api/conversations.replies",
            get(|State(s): State<Mock>, Query(q): Query<HashMap<String, String>>| async move {
                let s = s.lock().unwrap();
                let parent = s.messages.get(&q["channel"]).and_then(|l| l.iter().find(|m| m["ts"] == q["ts"].as_str()).cloned());
                let mut list: Vec<Value> = parent.into_iter().collect();
                list.extend(s.replies.get(&(q["channel"].clone(), q["ts"].clone())).cloned().unwrap_or_default());
                Json(json!({"ok": true, "messages": list, "has_more": false}))
            }),
        )
        .route(
            "/api/chat.postMessage",
            post(|State(s): State<Mock>, Json(b): Json<Value>| async move {
                let mut s = s.lock().unwrap();
                s.posted.push(b.clone());
                let mut message = json!({"type": "message", "user": "UME", "text": b["text"], "ts": ts(0, s.posted.len() as u32)});
                if b["thread_ts"].is_string() {
                    message["thread_ts"] = b["thread_ts"].clone();
                }
                Json(json!({"ok": true, "channel": b["channel"], "ts": message["ts"], "message": message}))
            }),
        )
        .route(
            "/api/reactions.add",
            post(|State(s): State<Mock>, Json(b): Json<Value>| async move {
                s.lock().unwrap().reactions.push(b);
                Json(json!({"ok": true}))
            }),
        )
        .route(
            "/api/conversations.mark",
            post(|State(s): State<Mock>, Json(b): Json<Value>| async move {
                s.lock().unwrap().marked.push(b);
                Json(json!({"ok": true}))
            }),
        )
        .route(
            "/files/{id}",
            get(|Path(id): Path<String>| async move {
                assert_eq!(id, "F1");
                "Release notes: the parser is twice as fast."
            }),
        )
        .with_state(s.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    s.lock().unwrap().base = url.clone();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    url
}

#[tokio::test]
async fn slack_mirrored_into_the_memex() {
    c6_memex::extract::set_override(Arc::new(c6_memex::extract::InProcess::default()));
    let s: Mock = Arc::default();
    let limited = Arc::new(AtomicBool::new(true));
    let base = slack(s.clone(), limited.clone()).await;
    let (m_a, m_b, r_1) = (ts(3600, 1), ts(1800, 2), ts(1700, 3));
    {
        let mut st = s.lock().unwrap();
        st.messages.insert(
            "C1".into(),
            vec![
                json!({"type": "message", "user": "U1", "ts": m_a, "text": "Shipping the release <@U2>, notes attached",
                       "files": [{"id": "F1", "name": "notes.txt", "mimetype": "text/plain", "size": 44, "url_private_download": format!("{base}/files/F1")}]}),
                json!({"type": "message", "user": "U2", "ts": m_b, "text": "Who reviews?", "thread_ts": m_b, "reply_count": 1, "latest_reply": r_1}),
                json!({"type": "message", "subtype": "channel_join", "user": "UME", "ts": ts(4000, 4), "text": "<@UME> has joined the channel"}),
            ],
        );
        st.replies.insert(
            ("C1".into(), m_b.clone()),
            vec![json!({"type": "message", "user": "U1", "ts": r_1, "thread_ts": m_b, "text": "I will"})],
        );
        st.messages.insert("D1".into(), vec![json!({"type": "message", "user": "U1", "ts": ts(600, 5), "text": "lunch?"})]);
        st.messages.insert(
            "C2".into(),
            (0..30)
                .map(|i| json!({"type": "message", "user": "U2", "ts": ts(7200 + i * 60, 10 + i as u32), "text": format!("big {i}")}))
                .collect(),
        );
    }
    // The slack kit's code, in this process, against the stand-in: a small
    // budget, so the import takes several runs; the re-read as the test says.
    let reread = Arc::new(AtomicI64::new(i64::MAX / 4));
    let (api_base, rr) = (format!("{base}/api"), reread.clone());
    c6_head::kitjobs::run_locally(
        "slack-api",
        Arc::new(move |run: c6_head::kitjobs::LocalRun| {
            let (api, reread) = (api_base.clone(), rr.load(Ordering::SeqCst));
            Box::pin(async move {
                let dir = tempfile::tempdir()?;
                for (path, data) in &run.inputs {
                    let p = dir.path().join(path);
                    std::fs::create_dir_all(p.parent().unwrap())?;
                    std::fs::write(p, data)?;
                }
                let accounts = run.env.iter().find(|(k, _)| k == "C6_ACCOUNTS").map(|(_, v)| v.clone()).unwrap_or_else(|| "[]".into());
                let env = c6_slack::Env { accounts, api, budget: 12, reread_every: reread };
                let done = c6_slack::run(&run.tool, dir.path(), &env).await;
                let mut files = std::collections::BTreeMap::new();
                let out = dir.path().join("out");
                let mut stack = vec![out.clone()];
                while let Some(d) = stack.pop() {
                    for e in std::fs::read_dir(&d)?.flatten() {
                        if e.file_type()?.is_dir() {
                            stack.push(e.path());
                        } else {
                            files.insert(e.path().strip_prefix(&out)?.to_string_lossy().into_owned(), std::fs::read(e.path())?);
                        }
                    }
                }
                let (exit, stderr) = match done {
                    Ok(()) => (0, String::new()),
                    Err(e) => (1, format!("{e:#}")),
                };
                Ok(c6_head::kitjobs::Output { exit: Some(exit), stdout: String::new(), stderr, files })
            })
        }),
    );
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
    let setup = || {
        let (http, url) = (http.clone(), format!("{api}/slack"));
        async move { http.get(url).send().await.unwrap().json::<Value>().await.unwrap() }
    };
    let rpc = |method: &str, params: Value| {
        let (http, url, body) = (http.clone(), format!("{api}/memex/rpc"), json!({"method": method, "params": params}));
        async move {
            let r = http.post(url).json(&body).send().await.unwrap();
            assert!(r.status().is_success(), "{}", r.text().await.unwrap());
            r.json::<Value>().await.unwrap()
        }
    };

    // The app: offered for you to approve; your Slack app made from the
    // manifest, which asks for user scopes and names this head.
    assert_eq!(http.post(format!("{api}/slack/connect")).send().await.unwrap().status(), reqwest::StatusCode::BAD_REQUEST, "no app yet");
    let install: Value = http.post(format!("{api}/slack/install")).send().await.unwrap().json().await.unwrap();
    let r = http.post(format!("{api}/decisions/{}/answer", install["decision"])).json(&json!({"option": "Install"})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let before = setup().await;
    assert_eq!((before["installed"].as_bool(), before["has_secret"].as_bool()), (Some(true), Some(false)), "{before}");
    let manifest: Value = serde_json::from_str(before["manifest"].as_str().unwrap()).unwrap();
    assert!(manifest["oauth_config"]["scopes"]["user"].as_array().unwrap().iter().any(|x| x == "im:history"), "{manifest}");
    assert_eq!(manifest["oauth_config"]["redirect_urls"][0], before["redirect_uri"]);
    let r = http.put(format!("{api}/slack/client")).json(&json!({"client_id": "cid", "client_secret": "csecret"})).send().await.unwrap();
    assert!(r.status().is_success());
    let go: Value = http.post(format!("{api}/slack/connect")).send().await.unwrap().json().await.unwrap();
    let q: HashMap<String, String> = url::Url::parse(go["authorize_url"].as_str().unwrap()).unwrap().query_pairs().into_owned().collect();
    assert!(q["user_scope"].contains("im:history,") && q["client_id"] == "cid" && !q.contains_key("scope"), "{q:?}");
    // (The sign-in itself is the providers test's; here, the workspace it keeps.)
    let kept = c6_head::providers::Kept {
        realm: "home",
        instance: "slack",
        provider: "slack",
        account: "T1",
        name: "Acme",
        refresh: "",
        access: "xoxp-1",
        expires: i64::MAX / 2,
        scopes: "all",
    };
    c6_head::providers::keep(&head.state, &kept, None).await.unwrap();
    c6_head::memex::reload(&head.state).await;

    // The first run is told to slow down: it stops, and says so.
    assert!(http.post(format!("{api}/slack/sync")).send().await.unwrap().status().is_success());
    eventually("the rate limit noted", async || setup().await["workspaces"][0]["retry_after"].as_i64().map(|_| ())).await;
    assert!(!limited.load(Ordering::SeqCst));
    // Then it imports, over several runs (each kept to its budget).
    assert!(http.post(format!("{api}/slack/sync")).send().await.unwrap().status().is_success());
    let w = eventually("the import", async || {
        let w = setup().await["workspaces"][0].clone();
        (w["imported"] == 3 && w["files_waiting"] == 0).then_some(w)
    })
    .await;
    assert_eq!((w["conversations"].as_u64(), w["people"].as_u64(), w["name"].as_str()), (Some(3), Some(3), Some("Acme")), "{w}");

    // Conversations, messages (a thread's replies under it), threads.
    let chans = rpc("memex.chat.channels", json!({})).await;
    let names: Vec<&str> = chans.as_array().unwrap().iter().map(|c| c["channel_name"].as_str().unwrap()).collect();
    assert_eq!(names, ["@Ana Lima", "#eng", "#big"], "the most recently active first: {chans}");
    let eng = rpc("memex.chat.messages", json!({"workspace": "T1", "channel": "C1"})).await;
    let texts: Vec<&str> = eng.as_array().unwrap().iter().map(|m| m["text"].as_str().unwrap()).collect();
    assert_eq!(texts, ["Bo: Who reviews?", "Ana Lima: Shipping the release @Bo, notes attached"], "no joins, no replies: {eng}");
    assert_eq!(eng[0]["meta"]["reply_count"], 1);
    assert!(eng[1]["url"].as_str().unwrap().starts_with("https://acme.slack.com/archives/C1/p"), "{eng}");
    let thread = rpc("memex.chat.thread", json!({"workspace": "T1", "channel": "C1", "thread_ts": m_b})).await;
    assert_eq!(
        thread.as_array().unwrap().iter().map(|m| m["text"].as_str().unwrap()).collect::<Vec<_>>(),
        ["Bo: Who reviews?", "Ana Lima: I will"]
    );
    eventually("every page of #big imported", async || {
        let big = rpc("memex.chat.messages", json!({"workspace": "T1", "channel": "C2", "limit": 100})).await;
        (big.as_array()?.len() == 30).then_some(())
    })
    .await;
    // Who said it and where: Ana is the person mail knows (by email); Bo has
    // no email, so he is Slack's.
    let got: Value = http.get(format!("{api}/memex/get?id={}", eng[1]["id"].as_str().unwrap())).send().await.unwrap().json().await.unwrap();
    let item = got["item"].clone();
    let about: Vec<&str> = item["about"].as_array().unwrap().iter().map(|t| t["label"].as_str().unwrap()).collect();
    for key in ["person:ana@acme.com", "slack-user:T1/U2", "channel:slack/T1/C1"] {
        assert!(about.contains(&key), "{key} in {about:?}");
    }
    let names: Vec<&str> = item["about"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert!(names.contains(&"Ana Lima") && names.contains(&"#eng"), "{names:?}");
    // The shared document, read into the memex.
    let found = eventually("the document read", async || {
        let v: Value = http.get(format!("{api}/memex/search?q=parser%20twice%20fast")).send().await.ok()?.json().await.ok()?;
        v.to_string().contains("notes.txt").then_some(v)
    })
    .await;
    assert!(found.to_string().contains("document"), "{found}");

    // Keeping up: a new message.
    s.lock().unwrap().messages.get_mut("C1").unwrap().push(json!({"type": "message", "user": "U1", "ts": ts(5, 50), "text": "Merged."}));
    assert!(http.post(format!("{api}/slack/sync")).send().await.unwrap().status().is_success());
    eventually("the new message", async || {
        let eng = rpc("memex.chat.messages", json!({"workspace": "T1", "channel": "C1"})).await;
        (eng[0]["text"] == "Ana Lima: Merged.").then_some(())
    })
    .await;

    // Edits and deletions, caught by reading the last week again.
    {
        let mut st = s.lock().unwrap();
        let c1 = st.messages.get_mut("C1").unwrap();
        c1.retain(|m| m["ts"] != m_b.as_str());
        let a = c1.iter_mut().find(|m| m["ts"] == m_a.as_str()).unwrap();
        a["text"] = json!("Shipping the release tomorrow");
        a["edited"] = json!({"user": "U1", "ts": ts(1, 60)});
    }
    reread.store(0, Ordering::SeqCst);
    assert!(http.post(format!("{api}/slack/sync")).send().await.unwrap().status().is_success());
    let eng = eventually("the edit and the deletion", async || {
        let eng = rpc("memex.chat.messages", json!({"workspace": "T1", "channel": "C1"})).await;
        let texts: Vec<String> = eng.as_array()?.iter().map(|m| m["text"].as_str().unwrap_or("").to_owned()).collect();
        (texts == ["Ana Lima: Merged.", "Ana Lima: Shipping the release tomorrow"]).then_some(eng)
    })
    .await;
    assert_eq!(eng[1]["meta"]["edited"], true);
    reread.store(i64::MAX / 4, Ordering::SeqCst);

    // Posting as you (the Chat page): it shows at once.
    let posted = rpc("memex.chat.post", json!({"workspace": "T1", "channel": "C1", "text": "On it", "thread_ts": m_a})).await;
    assert!(posted.to_string().contains("ts"), "{posted}");
    assert_eq!(s.lock().unwrap().posted[0]["thread_ts"], m_a.as_str());
    let thread = rpc("memex.chat.thread", json!({"workspace": "T1", "channel": "C1", "thread_ts": m_a})).await;
    assert!(thread.to_string().contains("Me Myself: On it"), "{thread}");
    rpc("memex.chat.react", json!({"workspace": "T1", "channel": "C1", "ts": m_a, "name": ":tada:"})).await;
    assert_eq!(s.lock().unwrap().reactions[0]["name"], "tada");

    // An agent in a chat session: it reads the conversation, and its post
    // waits for you.
    let put = |path: &str, v: Value| {
        let (http, url) = (http.clone(), format!("{api}/settings/{path}"));
        async move { http.put(url).json(&v).send().await.unwrap().status() }
    };
    assert!(put("memex_scopes", json!([{"name": "Chat sessions", "applies_to": ["chat"], "kinds": ["chat"]}])).await.is_success());
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(dir.path()).unwrap();
    let worker = Worker::open(dir.path(), Identity::load(dir.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    mock.push(Reply::ToolCalls(vec![("chat_messages".into(), r#"{"workspace": "T1", "channel": "C1"}"#.into())]));
    mock.push(Reply::ToolCalls(vec![(
        "chat_post".into(),
        r#"{"workspace": "T1", "channel": "C1", "text": "Reviewed, looks good."}"#.into(),
    )]));
    mock.push(Reply::Text("Posted.".into()));
    let session: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "answer #eng", "labels": ["chat"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let call = eventually("the approval request", async || {
        let evs: Vec<Event> = http.get(format!("{api}/sessions/{}/events", session.id)).send().await.ok()?.json().await.ok()?;
        evs.iter().find_map(|e| match &e.kind {
            EventKind::Approval { call_id, state, tool, .. } if state == "requested" && tool == "chat_post" => Some(call_id.clone()),
            _ => None,
        })
    })
    .await;
    let read = mock.requests()[1]["messages"].to_string();
    assert!(read.contains("untrusted data from chat") && read.contains("Shipping the release tomorrow"), "{read}");
    assert_eq!(s.lock().unwrap().posted.len(), 1, "nothing posted before you say so");
    http.post(format!("{api}/sessions/{}/approvals/{call}", session.id)).json(&json!({"allow": true})).send().await.unwrap();
    eventually("the agent's post", async || (s.lock().unwrap().posted.len() == 2).then_some(())).await;
    assert_eq!(s.lock().unwrap().posted[1]["text"], "Reviewed, looks good.");

    // Another realm has none of it.
    assert!(http.post(format!("{api}/realms")).json(&json!({"id": "lab"})).send().await.unwrap().status().is_success());
    let lab = http
        .post(format!("{api}/memex/rpc?realm=lab"))
        .json(&json!({"method": "memex.chat.channels", "params": {}}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(lab, json!([]));

    stop.cancel();
    head.shutdown().await.unwrap();
}
