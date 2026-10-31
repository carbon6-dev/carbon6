//! Work (docs/work.md): a session you start joins the work an agent
//! guesses (or the one you chose); its state follows what's attached; an
//! agent keeps "where was I" and offers "looks done", and only you close
//! it; quiet work nudges you once; what's open is grouped on request.

use c6_api::work::WorkItem;
use c6_api::{Connection, Session};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// The guess's prompt, the note's and the grouping's (their defaults).
const GUESS: &str = "which piece of work it belongs to";
const NOTE: &str = "Keep the user's note on a piece of work";
const GROUP: &str = "Group what is open below";
/// In every session's requests, never in work's.
const SESSION: &str = "You are carbon6";
const TOOL_RESULT: &str = "\"role\":\"tool\"";

#[tokio::test]
async fn work_follows_what_belongs_to_it() {
    let mock = MockLlm::start().await;
    let (head_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let wid = worker.identity.meta.worker_id.0;
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // Work's agent steps go to a small model.
    let r = http
        .put(format!("{api}/settings/routes"))
        .json(&json!([{"name": "work", "applies_to": ["task:work"], "connection": conn.id, "model": "small", "enabled": true}]))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let start = |body: Value| {
        let (http, api, conn, wid) = (http.clone(), api.clone(), conn.id.clone(), wid);
        async move {
            let mut body = body;
            body["worker_id"] = json!(wid);
            body["cwd"] = json!("");
            body["connection_id"] = json!(conn);
            body["model"] = json!("m");
            let s: Session = http.post(format!("{api}/sessions")).json(&body).send().await.unwrap().json().await.unwrap();
            s
        }
    };
    let idle = |id: String, turns: usize| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            eventually("the turn to end", async || {
                let d: Value = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
                let ev: Vec<Value> = http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok()?;
                let ended = ev.iter().filter(|e| e["type"] == "turn_ended").count();
                (d["session"]["status"] == "idle" && ended >= turns).then_some(())
            })
            .await
        }
    };
    let of = |id: String| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            let w: Vec<WorkItem> =
                http.get(format!("{api}/work/of")).query(&[("ref", format!("session:{id}"))]).send().await.unwrap().json().await.unwrap();
            w
        }
    };
    let item = |id: String| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            let w: WorkItem = http.get(format!("{api}/work/{id}")).send().await.unwrap().json().await.unwrap();
            w
        }
    };
    let note =
        |next: &str| Reply::Text(json!({"done": "Fixed the pager", "left": "the footer", "next": next, "finished": false}).to_string());

    // 1. A session you start: the agent guesses new work for it.
    mock.push_about(GUESS, Reply::Text(r#"{"new": "Fix the pager", "done_when": "the pager test passes"}"#.into()));
    mock.push_about_all(&[SESSION, "fix the pager bug"], Reply::Text("Fixed the off-by-one in the pager.".into()));
    for _ in 0..3 {
        mock.push_about(NOTE, note("Fix the footer"));
    }
    let a = start(json!({"prompt": "fix the pager bug", "work": "guess"})).await;
    let w = eventually("the guessed work", async || of(a.id.clone()).await.into_iter().next()).await;
    assert_eq!(w.fields.title, "Fix the pager");
    assert_eq!(w.fields.done_when, "the pager test passes");
    assert_eq!(w.fields.attached[0].by, "guess");
    let guessed = mock.requests().into_iter().find(|r| r["messages"].to_string().contains(GUESS)).unwrap();
    assert_eq!(guessed["model"], "small", "the routing rule's model");
    // Its turn ends: your move, and the note says where things stand.
    idle(a.id.clone(), 1).await;
    // The note written after the turn ended (one may also have been written
    // while it ran, from the guess): it read the session's answer.
    let w = eventually("the note after the turn", async || {
        let read_it = mock.requests().iter().any(|r| {
            let m = r["messages"].to_string();
            m.contains(NOTE) && m.contains("Fixed the off-by-one")
        });
        let w = item(w.id.clone()).await;
        (read_it && w.fields.note.as_ref().is_some_and(|n| n.next == "Fix the footer") && !c6_head::work::writing(&head.state).await)
            .then_some(w)
    })
    .await;
    assert_eq!(w.state, "needs_you", "{}", w.why);
    assert!(w.why.contains("finished its turn"), "{}", w.why);

    // 2. One you start in that work joins it without a guess; its agent
    // attaches the pull request it opened, waiting for review.
    let guesses = mock.requests().iter().filter(|r| r["messages"].to_string().contains(GUESS)).count();
    mock.push_about_all(
        &[SESSION, "also fix the footer"],
        Reply::ToolCalls(vec![(
            "work".into(),
            json!({"action": "attach", "ref": "https://github.com/o/r/pull/7", "title": "PR #7", "status": "waiting"}).to_string(),
        )]),
    );
    mock.push_about_all(&[SESSION, "PR #7", TOOL_RESULT], Reply::Text("Opened PR #7 for review.".into()));
    for _ in 0..4 {
        mock.push_about(NOTE, note("Wait for the review of PR #7"));
    }
    let b = start(json!({"prompt": "also fix the footer", "work": w.id})).await;
    idle(b.id.clone(), 1).await;
    let w = eventually("the pull request", async || {
        let w = item(w.id.clone()).await;
        (w.fields.attached.len() == 3).then_some(w)
    })
    .await;
    let refs: Vec<&str> = w.fields.attached.iter().map(|a| a.reference.as_str()).collect();
    assert_eq!(refs, [format!("session:{}", a.id).as_str(), &format!("session:{}", b.id), "https://github.com/o/r/pull/7"]);
    assert_eq!(w.fields.attached[2].by, format!("session:{}", b.id));
    assert_eq!(w.state, "waiting", "others are on it: {}", w.why);
    assert_eq!(mock.requests().iter().filter(|r| r["messages"].to_string().contains(GUESS)).count(), guesses, "no guess");

    // 3. Merged: the agent offers "looks done", and only you close it.
    mock.push_about_all(
        &[SESSION, "PR #7 is merged"],
        Reply::ToolCalls(vec![(
            "work".into(),
            json!({"action": "attach", "ref": "https://github.com/o/r/pull/7", "status": "done", "detail": "merged into main"}).to_string(),
        )]),
    );
    mock.push_about_all(&[SESSION, "PR #7 is merged", TOOL_RESULT], Reply::Text("Noted.".into()));
    let say = http.post(format!("{api}/sessions/{}/messages", b.id)).json(&json!({"text": "PR #7 is merged"})).send().await.unwrap();
    assert!(say.status().is_success());
    idle(b.id.clone(), 2).await;
    for _ in 0..3 {
        mock.push_about_all(&[NOTE, "merged into main"], Reply::Text(json!({"done": "Both fixed, PR #7 merged", "left": "", "next": "Close it", "finished": true, "evidence": "PR #7 was merged into main"}).to_string()));
    }
    let w = eventually("the offer", async || {
        let _ = http.post(format!("{api}/work/{}/refresh", w.id)).send().await;
        let w = item(w.id.clone()).await;
        w.fields.offer.is_some().then_some(w)
    })
    .await;
    assert_eq!(w.state, "needs_you");
    assert_eq!(w.why, "Looks done: PR #7 was merged into main");
    assert_eq!(w.fields.status, "open", "still yours to close");
    let r = http.patch(format!("{api}/work/{}", w.id)).json(&json!({"status": "done"})).send().await.unwrap();
    let closed: WorkItem = r.json().await.unwrap();
    assert_eq!((closed.state.as_str(), closed.fields.offer.is_none()), ("done", true));
    let open: Vec<WorkItem> = http.get(format!("{api}/work")).send().await.unwrap().json().await.unwrap();
    assert!(open.iter().all(|x| x.id != w.id));
    let done: Vec<WorkItem> = http.get(format!("{api}/work?closed=true")).send().await.unwrap().json().await.unwrap();
    assert!(done.iter().any(|x| x.id == w.id));
    // Its history keeps every version.
    let changes: Vec<Value> =
        http.get(format!("{api}/collections/work/records/{}/history", w.id)).send().await.unwrap().json().await.unwrap();
    assert!(changes.len() >= 4, "{changes:?}");

    // 4. A throwaway question is no work.
    mock.push_about_all(&[GUESS, "2+2"], Reply::Text(r#"{"none": true}"#.into()));
    mock.push_about_all(&[SESSION, "what's 2+2"], Reply::Text("4".into()));
    let c = start(json!({"prompt": "what's 2+2", "work": "guess"})).await;
    idle(c.id.clone(), 1).await;
    eventually("the guess", async || {
        mock.requests().iter().any(|r| r["messages"].to_string().contains("2+2") && r["messages"].to_string().contains(GUESS)).then_some(())
    })
    .await;
    assert!(of(c.id.clone()).await.is_empty());

    // 5. A to-do you add, and nothing moves for days: you're told once.
    let todo: WorkItem =
        http.post(format!("{api}/work")).json(&json!({"title": "Renew the domain"})).send().await.unwrap().json().await.unwrap();
    assert_eq!((todo.state.as_str(), todo.why.as_str()), ("open", "Nothing started yet"));
    let later = c6_core::now_ms() + 10 * 24 * 3600 * 1000;
    assert!(c6_head::work::sweep(&head.state, later).await.unwrap() >= 1);
    assert_eq!(c6_head::work::sweep(&head.state, later + 1000).await.unwrap(), 0, "once per state");

    // Without "guess" (an API or CLI caller), no model is asked.
    mock.push_about_all(&[SESSION, "list the files"], Reply::Text("Done.".into()));
    let e = start(json!({"prompt": "list the files"})).await;
    idle(e.id.clone(), 1).await;
    assert!(
        !mock.requests().iter().any(|r| r["messages"].to_string().contains(GUESS) && r["messages"].to_string().contains("list the files"))
    );
    assert!(of(e.id.clone()).await.is_empty());

    // 6. What's open, grouped into work for you to accept.
    mock.push_about_all(&[SESSION, "rewrite the docs"], Reply::Text("Started on the docs.".into()));
    let d = start(json!({"prompt": "rewrite the docs", "work": "none"})).await;
    idle(d.id.clone(), 1).await;
    let d_ref = format!("session:{}", d.id);
    mock.push_about(
        GROUP,
        Reply::Text(json!({"work": [{"title": "Docs rewrite", "done_when": "published", "refs": [d_ref, "session:nope"]}]}).to_string()),
    );
    let proposals: Vec<Value> = http.post(format!("{api}/work/proposals")).send().await.unwrap().json().await.unwrap();
    assert_eq!(proposals.len(), 1, "{proposals:?}");
    assert_eq!(proposals[0]["title"], "Docs rewrite");
    let refs: Vec<&str> = proposals[0]["attach"].as_array().unwrap().iter().filter_map(|a| a["ref"].as_str()).collect();
    assert_eq!(refs, [d_ref.as_str()], "only what's open and unfiled");
    for request in mock.requests().iter().filter(|r| r["model"] == "small") {
        assert_eq!(request["response_format"], json!({"type": "json_object"}));
    }
    stop.cancel();
}

/// To-dos (docs/work.md §12): you list a piece of work's steps, start one
/// as a session later, and it, or you, ticks it off; a session adds more.
#[tokio::test]
async fn todos_become_sessions_later() {
    let mock = MockLlm::start().await;
    let (head_dir, state) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    // No agent steps of work's own: every model call here is the session's.
    let r = http.put(format!("{api}/settings/work")).json(&json!({"guess": false, "notes": false})).send().await.unwrap();
    assert!(r.status().is_success());
    let send = |req: reqwest::RequestBuilder| async move {
        let r = req.send().await.unwrap();
        let status = r.status();
        let body: Value = r.json().await.unwrap();
        assert!(status.is_success(), "{status}: {body}");
        body
    };
    let item = |v: Value| serde_json::from_value::<WorkItem>(v).unwrap();

    // 1. You list the steps (the same open one twice: one to-do).
    let w = item(send(http.post(format!("{api}/work")).json(&json!({"title": "Ship the cache"}))).await);
    let todos = format!("{api}/work/{}/todos", w.id);
    send(http.post(&todos).json(&json!({"text": "Write the cache tests"}))).await;
    send(http.post(&todos).json(&json!({"text": "Document the flags", "prompt": "Add the cache flags to README.md."}))).await;
    let w = item(send(http.post(&todos).json(&json!({"text": "write the cache tests"}))).await);
    assert_eq!(w.fields.todos.len(), 2, "{w:?}");
    assert_eq!((w.state.as_str(), w.why.as_str()), ("open", "2 of 2 to-dos left: Write the cache tests"));
    let (tests, docs) = (w.fields.todos[0].id.clone(), w.fields.todos[1].id.clone());

    // 2. Later, one becomes a session: its prompt, and how to tick it off.
    let start: Value = send(http.get(format!("{todos}/{docs}/start"))).await;
    let first = start["text"].as_str().unwrap().to_owned();
    assert!(first.contains("Add the cache flags to README.md.") && first.contains("Ship the cache") && first.contains(&docs), "{first}");
    assert!(start["seed"].is_null(), "no session to start from yet");
    mock.push(Reply::ToolCalls(vec![(
        "work".into(),
        json!({"action": "todo_done", "id": docs, "evidence": "README.md lists the flags"}).to_string(),
    )]));
    mock.push(Reply::ToolCalls(vec![(
        "work".into(),
        json!({"action": "todo", "text": "Benchmark the cache", "prompt": "Run cargo bench -p cache and compare."}).to_string(),
    )]));
    mock.push(Reply::Text("Documented the flags; a benchmark is still worth running.".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": "", "connection_id": conn.id, "model": "m",
                      "prompt": first, "work": w.id, "todo": docs}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // 3. The session ticked its to-do off and added the next.
    let w = eventually("the session's to-dos", async || {
        let w = item(send(http.get(format!("{api}/work/{}", w.id))).await);
        (w.fields.todos.len() == 3).then_some(w)
    })
    .await;
    let by = format!("session:{}", s.id);
    let d = &w.fields.todos[1];
    assert_eq!((d.session.as_str(), d.done_by.as_str(), d.evidence.as_str()), (s.id.as_str(), by.as_str(), "README.md lists the flags"));
    assert!(d.done_at.is_some());
    let b = &w.fields.todos[2];
    assert_eq!(
        (b.text.as_str(), b.prompt.as_str(), b.by.as_str(), b.origin.as_str()),
        ("Benchmark the cache", "Run cargo bench -p cache and compare.", by.as_str(), s.id.as_str())
    );
    assert!(w.fields.attached.iter().any(|a| a.reference == by), "the session joined the work");
    assert_eq!(s.title, "Document the flags", "named after its to-do");
    let start: Value = send(http.get(format!("{todos}/{}/start", b.id))).await;
    assert_eq!(start["seed"], json!(s.id), "it starts where the session that raised it ran");

    // 4. You tick one off, untick it, change it, and take one away.
    let w = item(send(http.patch(format!("{todos}/{tests}")).json(&json!({"done": true}))).await);
    assert_eq!(w.fields.todos[0].done_by, "you");
    let w = item(send(http.patch(format!("{todos}/{tests}")).json(&json!({"done": false, "text": "Write the cache's tests"}))).await);
    assert!(w.fields.todos[0].done_at.is_none() && w.fields.todos[0].text == "Write the cache's tests", "{w:?}");
    let w = item(send(http.delete(format!("{todos}/{}", b.id))).await);
    assert_eq!(w.fields.todos.len(), 2);
    let r = http.patch(format!("{todos}/nope")).json(&json!({"done": true})).send().await.unwrap();
    assert_eq!(r.status(), 404);
    stop.cancel();
}
