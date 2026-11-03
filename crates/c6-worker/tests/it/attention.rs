//! What needs you (`GET /api/v1/attention`): a tool call waiting for
//! approval and an open decision wait on you until answered; a session's
//! ended turn is worth a look until you've seen it; another realm's
//! decision is only a count there, never its title; agents' answers to
//! your comments are one row per document; "Mark all seen" clears turns,
//! answers and the Brief until something new comes, in your realm only;
//! and each change pings the live feed.

use c6_api::docs::CommentNew;
use c6_api::{Attention, Connection, Session};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use reqwest::Method as M;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn what_needs_you_comes_and_goes() {
    let mock = MockLlm::start().await;
    let (head_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let head = c6_head::start(HeadConfig {
        data_dir: head_dir.path().to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: 0,
        worker_port: 0,
        auth: true,
        ..Default::default()
    })
    .await
    .unwrap();
    let port = head.ui_addrs[0].port();
    let api = format!("http://127.0.0.1:{port}/api/v1");
    let http = reqwest::Client::new();
    // Seeding with root's token; looking as your browser in home.
    let root = head.state.store.create_api_token("t", None).await.unwrap().token.unwrap();
    let link = head.state.store.create_login_link().await.unwrap();
    let signed_in = http.post(format!("{api}/auth/link")).json(&json!({"token": link})).send().await.unwrap();
    let cookie = signed_in.headers()["set-cookie"].to_str().unwrap().split(';').next().unwrap().to_owned();
    let send_in = |realm: &str, method: M, path: &str, body: Option<Value>| {
        let mut r = http.request(method, format!("{api}/{path}")).header("cookie", &cookie).header("x-c6-realm", realm);
        if let Some(b) = body {
            r = r.json(&b);
        }
        let r = r.send();
        async move {
            let r = r.await.unwrap();
            let s = r.status();
            let v: Value = r.json().await.unwrap_or(Value::Null);
            assert!(s.is_success(), "{s}: {v}");
            v
        }
    };
    let send = |method: M, path: &str, body: Option<Value>| send_in("home", method, path, body);
    let seed = |method: M, path: &str, body: Value| {
        let r = http.request(method, format!("{api}/{path}")).bearer_auth(&root).json(&body).send();
        async move {
            let r = r.await.unwrap();
            let s = r.status();
            let v: Value = r.json().await.unwrap_or(Value::Null);
            assert!(s.is_success(), "{s}: {v}");
            v
        }
    };
    let attention_in = |realm: &str| {
        let r = send_in(realm, M::GET, "attention", None);
        async move { serde_json::from_value::<Attention>(r.await).unwrap() }
    };
    let attention = || attention_in("home");

    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    seed(M::POST, "realms", json!({"id": "lab", "name": "Lab"})).await;
    let conn: Connection =
        serde_json::from_value(seed(M::POST, "connections", json!({"name": "m", "base_url": mock.base_url()})).await).unwrap();
    seed(M::PUT, "settings/policies", json!([{"name": "ask on deploy", "tools": "Bash", "pattern": "deploy", "action": "ask"}])).await;
    let a = attention().await;
    assert!(a.waiting.is_empty() && a.worth.is_empty() && a.elsewhere.is_empty(), "{a:?}");

    // The live feed, read as home's page does: how many pings came.
    let (pinged, pings) = watch::channel(0usize);
    {
        use futures::StreamExt;
        use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
        let mut req = format!("ws://127.0.0.1:{port}/api/v1/live?c6_realm=home").into_client_request().unwrap();
        req.headers_mut().insert("cookie", cookie.parse().unwrap());
        let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        // Its first snapshot says it's listening.
        loop {
            let Message::Text(t) = c6_testkit::within("the first snapshot", ws.next()).await.unwrap().unwrap() else { continue };
            if serde_json::from_str::<Value>(&t).unwrap()["type"] == "sessions" {
                break;
            }
        }
        tokio::spawn(async move {
            while let Some(Ok(m)) = ws.next().await {
                if let Message::Text(t) = m
                    && serde_json::from_str::<Value>(&t).is_ok_and(|v| v["type"] == "attention")
                {
                    pinged.send_modify(|n| *n += 1);
                }
            }
        });
    }
    // How many came so far; then waiting for one more than that.
    let so_far = || *pings.borrow();
    let pinged_since = |seen: usize| {
        let mut pings = pings.clone();
        async move {
            c6_testkit::within("a ping on the live feed", pings.wait_for(|n| *n > seen)).await.unwrap();
        }
    };

    // 1. A tool call held for approval waits on you, with its session and
    // call; while it waits, the session isn't also worth a look.
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo deploy-ran"}"#.into())]));
    mock.push(Reply::Text("Deployed.".into()));
    let s: Session = serde_json::from_value(
        send(
            M::POST,
            "sessions",
            Some(json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "deploy it", "title": "Deploy"})),
        )
        .await,
    )
    .unwrap();
    let approval =
        eventually("the approval to wait on you", async || attention().await.waiting.into_iter().find(|i| i.kind == "approval")).await;
    assert_eq!((approval.session_id.as_deref(), approval.detail.as_str()), (Some(s.id.as_str()), "Deploy"), "{approval:?}");
    assert!(approval.call_id.as_deref().is_some_and(|c| !c.is_empty()) && approval.key.starts_with("decision:"), "{approval:?}");
    pinged_since(0).await;
    assert!(attention().await.worth.is_empty());
    send(M::POST, &format!("decisions/{}/answer", approval.decision_id.unwrap()), Some(json!({"option": "Allow"}))).await;

    // 2. Its turn ended: worth a look, until you've seen it.
    let key = format!("session:{}", s.id);
    let turn = eventually("the ended turn to be worth a look", async || attention().await.worth.into_iter().find(|i| i.key == key)).await;
    assert_eq!((turn.kind.as_str(), turn.detail.as_str(), turn.session_id.as_deref()), ("turn_ended", "Deploy", Some(s.id.as_str())));
    let a = attention().await;
    assert!(a.waiting.is_empty(), "the approval left once answered: {a:?}");
    let n = so_far();
    send(M::POST, "attention/seen", Some(json!({"keys": [key]}))).await;
    assert!(attention().await.worth.is_empty());
    pinged_since(n).await;

    // 3. A decision opened here waits on you until it's answered.
    let n = so_far();
    let d = send(M::POST, "decisions", Some(json!({"title": "Ship on Friday?", "options": [{"label": "Yes"}, {"label": "No"}]}))).await;
    let waiting = attention().await.waiting;
    assert_eq!(waiting.iter().map(|i| (i.kind.as_str(), i.title.as_str())).collect::<Vec<_>>(), [("decision", "Ship on Friday?")]);
    pinged_since(n).await;
    let n = so_far();
    send(M::POST, &format!("decisions/{}/answer", d["id"]), Some(json!({"option": "Yes"}))).await;
    assert!(attention().await.waiting.is_empty());
    pinged_since(n).await;

    // 4. Another realm's decision: only a count, from here.
    let n = so_far();
    seed(M::POST, "decisions?realm=lab", json!({"title": "Lab's secret plan"})).await;
    let a = attention().await;
    assert!(a.waiting.is_empty(), "{a:?}");
    assert_eq!(a.elsewhere.iter().map(|e| (e.id.as_str(), e.name.as_str(), e.waiting)).collect::<Vec<_>>(), [("lab", "Lab", 1)]);
    assert!(!serde_json::to_string(&a).unwrap().contains("secret"), "{a:?}");
    pinged_since(n).await;
    // In lab it waits in full.
    let lab = attention_in("lab").await;
    assert_eq!(lab.waiting[0].title, "Lab's secret plan", "{lab:?}");

    // 5. Agents answer two of your comments on one document: one row for
    // it, with how many.
    let new_doc = async |realm: &str, title: &str| {
        let markdown = "# Pricing\n\nWe charge per seat.\n\nAnnual plans get two months free.";
        let d = send_in(realm, M::POST, "docs", Some(json!({"title": title, "markdown": markdown}))).await;
        let blocks: Vec<String> = d["tabs"][0]["blocks"].as_array().unwrap().iter().map(|b| b["id"].as_str().unwrap().to_owned()).collect();
        (d["id"].as_str().unwrap().to_owned(), blocks)
    };
    let comment = async |realm: &str, doc: &str, block: &str, text: &str| {
        let t = send_in(realm, M::POST, &format!("docs/{doc}/threads"), Some(json!({"anchor": {"block": block}, "text": text}))).await;
        (t["id"].as_str().unwrap().to_owned(), t["about"].as_str().unwrap().to_owned())
    };
    let by = format!("session:{}", s.id);
    let answer = async |realm: &str, doc: &str, thread: &str| {
        let c = CommentNew { text: "Done.".into(), to: String::new() };
        c6_head::docs::comments::reply(&head.state, realm, doc, thread, &c, &by).await.unwrap();
    };
    let answers = |a: &Attention| a.worth.iter().filter(|i| i.kind == "comment_answer").cloned().collect::<Vec<_>>();
    let (doc, blocks) = new_doc("home", "Pricing").await;
    let (t1, about1) = comment("home", &doc, &blocks[1], "Say the price.").await;
    let (t2, _) = comment("home", &doc, &blocks[2], "Two months or one?").await;
    assert!(answers(&attention().await).is_empty(), "nothing answered yet");
    answer("home", &doc, &t1).await;
    answer("home", &doc, &t2).await;
    let rows = answers(&attention().await);
    assert_eq!(rows.len(), 1, "one row for the document: {rows:?}");
    let r = &rows[0];
    assert_eq!(r.key, format!("doc:{doc}"));
    assert_eq!((r.title.as_str(), r.detail.as_str(), r.count), ("Pricing", "2 answers", Some(2)));
    assert_eq!((r.doc_id.as_deref(), r.thread_id.as_deref()), (Some(doc.as_str()), None));

    // 6. Lab's answered comment is lab's: not here, and not seen from here.
    let (lab_doc, lab_blocks) = new_doc("lab", "Lab pricing").await;
    let (lab_t, _) = comment("lab", &lab_doc, &lab_blocks[1], "Cheaper?").await;
    answer("lab", &lab_doc, &lab_t).await;
    let lab_key = format!("doc:{lab_doc}");
    assert!(answers(&attention().await).iter().all(|i| i.key != lab_key));
    assert!(answers(&attention_in("lab").await).iter().any(|i| i.key == lab_key));
    send(M::POST, "attention/seen", Some(json!({"keys": [lab_key]}))).await;
    assert!(answers(&attention_in("lab").await).iter().any(|i| i.key == lab_key), "home can't mark lab's seen");

    // 7. A turn that ended, the document's answers and someone waiting on
    // you in the Brief: "Mark all seen" clears all three.
    mock.push(Reply::Text("Hello.".into()));
    let s2: Session = serde_json::from_value(
        send(
            M::POST,
            "sessions",
            Some(json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": "", "connection_id": conn.id, "model": "m",
                        "prompt": "say hello", "title": "Greet"})),
        )
        .await,
    )
    .unwrap();
    let memex = c6_head::knowledge::get(&head.state).expect("the memex").of_realm("home").await.unwrap();
    let ask = async |person: &str, at: i64| {
        sqlx::query(
            "INSERT INTO requests (person, title, category, created_at, updated_at) VALUES (?, 'Send the contract', 'needs_you', ?, ?)",
        )
        .bind(person)
        .bind(at)
        .bind(at)
        .execute(&memex.store.pool)
        .await
        .unwrap();
    };
    ask("pat@example.net", c6_core::now_ms() - 60_000).await;
    let turn_key = format!("session:{}", s2.id);
    let a = eventually("the turn, the answers and the Brief to be worth a look", async || {
        let a = attention().await;
        let keys: Vec<&str> = a.worth.iter().map(|i| i.key.as_str()).collect();
        (keys.contains(&turn_key.as_str()) && keys.contains(&format!("doc:{doc}").as_str()) && keys.contains(&"brief")).then_some(a)
    })
    .await;
    assert_eq!(a.worth.iter().find(|i| i.key == "brief").and_then(|i| i.count), Some(1), "{a:?}");
    let n = so_far();
    send(M::POST, "attention/seen", Some(json!({}))).await;
    let a = attention().await;
    assert!(a.worth.is_empty(), "all seen: {a:?}");
    pinged_since(n).await;
    assert!(answers(&attention_in("lab").await).iter().any(|i| i.key == lab_key), "home's \"Mark all seen\" leaves lab's");

    // 8. A new answer in a seen thread brings the document back, with just
    // that thread; a newer request brings the Brief back. Opening each
    // (its key) sees it again.
    answer("home", &doc, &t1).await;
    let rows = answers(&attention().await);
    assert_eq!(rows.len(), 1, "{rows:?}");
    let r = &rows[0];
    assert_eq!((r.title.as_str(), r.detail.as_str(), r.count), ("Pricing", about1.as_str(), Some(1)));
    assert_eq!(r.thread_id.as_deref(), Some(t1.as_str()), "just the one thread");
    ask("sam@example.net", c6_core::now_ms()).await;
    let brief = attention().await.worth.into_iter().find(|i| i.key == "brief").expect("a newer request is worth a look");
    assert_eq!(brief.count, Some(2));
    send(M::POST, "attention/seen", Some(json!({"keys": [format!("doc:{doc}"), "brief"]}))).await;
    let a = attention().await;
    assert!(a.worth.is_empty(), "{a:?}");
    stop.cancel();
}
