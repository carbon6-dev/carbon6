//! The memex, part of the head (docs/knowledge.md); a separate c6m is turned away;
//! carbon6's sessions and memory notes flow into it; search finds them by
//! keyword and meaning; new sessions start with a brief; agents search and
//! write notes through the head, within their scope.

use std::time::Duration;

use c6_api::{Connection, Session, Worker as ApiWorker};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

async fn eventually<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Enrolls a node and keeps it attached; `memex`: with the memex on top.
async fn node(head: &c6_head::Head, _memex: bool, stop: &CancellationToken) -> (std::sync::Arc<Worker>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(dir.path()).unwrap();
    let w = Worker::open(dir.path(), Identity::load(dir.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();
    (w, dir)
}

#[tokio::test]
async fn memex_round_trip() {
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
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let put = |path: &str, v: Value| {
        let (http, url) = (http.clone(), format!("{api}/settings/{path}"));
        async move { http.put(url).json(&v).send().await.unwrap().status() }
    };
    assert_eq!(put("memex", json!({"embedding": {"connection": "nope", "model": "e"}})).await, reqwest::StatusCode::BAD_REQUEST);
    assert!(put("memex", json!({"embedding": {"connection": "m", "model": "embed-1"}})).await.is_success());
    assert_eq!(put("memex_scopes", json!([{"name": "x", "kinds": ["letters"]}])).await, reqwest::StatusCode::BAD_REQUEST);
    assert!(
        put("memex_scopes", json!([{"name": "Mail sessions see email", "applies_to": ["mail"], "kinds": ["email"]}])).await.is_success()
    );

    // A memory note made before there is a memex goes to it when it comes.
    let early =
        http.post(format!("{api}/memories")).json(&json!({"text": "Invoices are due on the 5th", "scope": ""})).send().await.unwrap();
    assert!(early.status().is_success(), "{}", early.text().await.unwrap());

    let (worker, _wd) = node(&head, false, &stop).await;
    // The memex is part of the head (docs/knowledge.md): a separate c6m is
    // turned away.
    let dir = tempfile::tempdir().unwrap();
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(dir.path()).unwrap();
    let w = Worker::open(dir.path(), Identity::load(dir.path()).unwrap()).await.unwrap();
    let m = c6_memex::Memex::open(&dir.path().join("memex"), w.link.clone()).await.unwrap();
    w.set_extension(m);
    let (tx, rx) = watch::channel(ConnState::Connecting);
    let refused = CancellationToken::new();
    tokio::spawn(conn::run(w.clone(), tx, refused.clone()));
    eventually("c6m turned away", async || {
        let s = rx.borrow().clone();
        (!matches!(s, ConnState::Attached { .. } | ConnState::Connecting)).then_some(())
    })
    .await;
    refused.cancel();
    let workers: Vec<ApiWorker> = http.get(format!("{api}/workers")).send().await.unwrap().json().await.unwrap();
    assert!(workers.iter().all(|w| w.role != "memex" || w.status != c6_api::WorkerStatus::Online), "{workers:?}");

    let wid = worker.identity.meta.worker_id.0;
    let create = |labels: Value, prompt: &str| {
        let (http, url) = (http.clone(), format!("{api}/sessions"));
        let body = json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": prompt, "labels": labels});
        async move { http.post(url).json(&body).send().await.unwrap().json::<Session>().await.unwrap() }
    };
    let status = || {
        let (http, url) = (http.clone(), format!("{api}/memex/status"));
        async move { http.get(url).send().await.unwrap().json::<c6_api::MemexStatus>().await.unwrap() }
    };
    let idle = |id: String| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            eventually("idle", async || {
                let d: Value = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
                (d["session"]["status"] == "idle").then_some(())
            })
            .await
        }
    };

    // 1: a session about client:acme; the agent remembers something.
    mock.push(Reply::ToolCalls(vec![(
        "remember".into(),
        r#"{"text": "Acme deploys go through the staging gate first", "scope": "client:acme"}"#.into(),
    )]));
    mock.push(Reply::Text("Noted. The deployment checklist lives in the ops runbook.".into()));
    let s1 = create(json!(["client:acme"]), "How do Acme deploys work?").await;
    idle(s1.id.clone()).await;
    let st = eventually("the session and notes in the memex", async || {
        let s = status().await;
        (s.items.get("session") == Some(&1) && s.items.get("note") == Some(&2)).then_some(s)
    })
    .await;
    assert!(st.connected && st.things >= 2, "{st:?}");
    eventually("every passage embedded", async || {
        let s = status().await;
        (s.passages > 0 && s.embedded == s.passages && s.embedding_model == "embed-1").then_some(())
    })
    .await;

    // Search (as you, through the API): keywords, and meaning.
    let search = |q: &str| {
        let (http, url) = (http.clone(), format!("{api}/memex/search?q={}", q.replace(' ', "%20")));
        async move { http.get(url).send().await.unwrap().json::<Vec<c6_api::MemexHit>>().await.unwrap() }
    };
    let hits = search("staging gate").await;
    assert_eq!(hits[0].kind, "note", "{hits:?}");
    assert!(hits[0].matched.contains(&"keyword".to_owned()) && hits[0].matched.contains(&"meaning".to_owned()), "{hits:?}");
    let hits = search("ops runbook").await;
    assert!(hits.iter().any(|h| h.kind == "session" && h.external_id == s1.id), "{hits:?}");
    let thing: Value = http.get(format!("{api}/memex/get?id=client:acme")).send().await.unwrap().json().await.unwrap();
    assert!(thing["thing"]["notes"].to_string().contains("staging gate"), "{thing}");
    assert!(thing["thing"]["items"].to_string().contains(&s1.id), "{thing}");

    // 2: a new session about Acme starts with a brief; its agent searches
    // the memex and saves a note.
    let requests_before = mock.requests().len();
    mock.push(Reply::ToolCalls(vec![("memex_search".into(), r#"{"query": "deployment checklist"}"#.into())]));
    mock.push(Reply::ToolCalls(vec![("memex_note".into(), r#"{"text": "Acme's staging gate needs a sign-off from Priya"}"#.into())]));
    mock.push(Reply::Text("Done.".into()));
    let s2 = create(json!(["client:acme"]), "Prepare the Acme deploy").await;
    eventually("session 2's three model calls", async || (mock.requests().len() >= requests_before + 3).then_some(())).await;
    idle(s2.id.clone()).await;
    let reqs = mock.requests();
    let system = reqs[requests_before]["messages"][0]["content"].to_string();
    // Memory notes come by their scope in their own section; the brief adds
    // what the knowledge store knows (without repeating them).
    assert!(system.contains("# Memex") && system.contains("# Memory"), "{system}");
    assert!(system.contains("staging gate") && system.contains("Invoices are due"), "{system}");
    assert_eq!(system.matches("Invoices are due").count(), 1, "not twice: {system}");
    let found = reqs[requests_before + 1]["messages"].to_string();
    assert!(found.contains("ops runbook") && found.contains(&s1.id), "the agent found session 1: {found}");
    let saved = reqs[requests_before + 2]["messages"].to_string();
    assert!(saved.contains("Saved note"), "{saved}");
    let notes = eventually("the agent's note", async || {
        let h = search("Priya").await;
        (!h.is_empty()).then_some(h)
    })
    .await;
    assert!(notes[0].source.starts_with("session:"), "attributed to the session: {notes:?}");
    let acme: Value = http.get(format!("{api}/memex/get?id=client:acme")).send().await.unwrap().json().await.unwrap();
    assert!(acme["thing"]["notes"].to_string().contains("Priya"), "about the session's labels: {acme}");

    // Scopes: an email is there, but only sessions labelled mail see it.
    c6_head::memex::call(
        &head.state,
        "memex.ingest",
        json!({"kind": "email", "source": "test", "external_id": "m1", "title": "Invoice 7781", "text": "Your invoice 7781 for October is attached."}),
    )
    .await
    .unwrap();
    let ask = |labels: Value| {
        let mock = mock.clone();
        let create = &create;
        async move {
            let before = mock.requests().len();
            mock.push(Reply::ToolCalls(vec![("memex_search".into(), r#"{"query": "invoice 7781"}"#.into())]));
            mock.push(Reply::Text("ok".into()));
            let s = create(labels, "find the invoice").await;
            eventually("the search result", async || (mock.requests().len() >= before + 2).then_some(())).await;
            let _ = s;
            mock.requests()[before + 1]["messages"].to_string()
        }
    };
    let plain = ask(json!([])).await;
    assert!(!plain.contains("Invoice 7781"), "{plain}");
    let mail = ask(json!(["mail"])).await;
    assert!(mail.contains("Invoice 7781"), "{mail}");

    // @memex:<label> in a message attaches what the memex knows about it.
    let before = mock.requests().len();
    mock.push(Reply::Text("ok".into()));
    create(json!([]), "Summarize @memex:client:acme please").await;
    eventually("the mention's turn", async || (mock.requests().len() > before).then_some(())).await;
    let sent = mock.requests()[before]["messages"].to_string();
    assert!(sent.contains("@memex:client:acme") && sent.contains("Priya") && sent.contains("staging gate"), "{sent}");

    // Forgetting a note removes it from the memex.
    let mems: Vec<c6_api::Memory> = http.get(format!("{api}/memories")).send().await.unwrap().json().await.unwrap();
    let acme_note = mems.iter().find(|m| m.text.contains("staging gate")).unwrap();
    assert!(http.delete(format!("{api}/memories/{}", acme_note.id)).send().await.unwrap().status().is_success());
    eventually("the note gone from the memex", async || {
        search("staging gate first")
            .await
            .iter()
            .all(|h| !(h.kind == "note" && h.source == "memory" && h.title.contains("staging gate")))
            .then_some(())
    })
    .await;

    stop.cancel();
    head.shutdown().await.unwrap();
}
