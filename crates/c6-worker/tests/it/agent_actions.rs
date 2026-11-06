//! Agents act like you (docs/agent-actions.md): a session's `carbon6` tool
//! makes work, projects and to-dos freely, changes your things once you
//! allow it, deletes yours only with your OK each time, and never touches
//! administration.

use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use reqwest::Method as M;
use serde_json::{Value, json};

#[tokio::test]
async fn agents_make_and_change_things_by_your_rule() {
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
    let (tx, mut rx) = tokio::sync::watch::channel(ConnState::Connecting);
    let stop = tokio_util::sync::CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let send = |method: M, path: String, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.request(method, &url).json(&body).send().await.unwrap() }
    };
    let get = |path: String| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.get(&url).send().await.unwrap().json::<Value>().await.unwrap() }
    };
    // Only the session calls the model.
    let r =
        send(M::PUT, "settings/work".into(), json!({"guess": false, "notes": false, "quiet_days": 3, "nudge": false, "needs_hours": 4}))
            .await;
    assert!(r.status().is_success());
    let conn: Value = send(M::POST, "connections".into(), json!({"name": "m", "base_url": mock.base_url()})).await.json().await.unwrap();
    let turns = |id: String, n: usize| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            eventually("the turn to end", async || {
                let d: Value = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
                let ev: Vec<Value> = http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok()?;
                let ended = ev.iter().filter(|e| e["type"] == "turn_ended").count();
                (d["session"]["status"] == "idle" && ended >= n).then_some(ev)
            })
            .await
        }
    };
    let last_result = |ev: &[Value]| -> String {
        ev.iter().rev().find(|e| e["type"] == "tool_result").map(|e| e["output"].as_str().unwrap_or("").to_owned()).unwrap_or_default()
    };
    let tool = |input: Value| Reply::ToolCalls(vec![("carbon6".into(), input.to_string())]);

    // Yours: a project and a work item with a to-do.
    let p: Value = send(M::POST, "projects".into(), json!({"title": "Falcon Robustness", "plan": false})).await.json().await.unwrap();
    let pid = p["id"].as_str().unwrap().to_owned();
    let w: Value = send(M::POST, "work".into(), json!({"title": "Your work"})).await.json().await.unwrap();
    let wid = w["id"].as_str().unwrap().to_owned();
    let w: Value = send(M::POST, format!("work/{wid}/todos"), json!({"text": "Yours to do"})).await.json().await.unwrap();
    let your_todo = w["todos"][0]["id"].as_str().unwrap().to_owned();

    // 1. Finding says what each may do; making new work in your project is free.
    mock.push(tool(json!({"find": "new work in a project"})));
    mock.push(tool(
        json!({"op": "projects.work", "input": {"id": pid, "body": {"title": "Harden retries", "done_when": "no lost writes"}}}),
    ));
    mock.push(Reply::Text("Made it.".into()));
    let s: Value = send(
        M::POST,
        "sessions".into(),
        json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": "", "connection_id": conn["id"], "model": "m",
               "prompt": "create a new Work in the Falcon Robustness project", "work": "none"}),
    )
    .await
    .json()
    .await
    .unwrap();
    let sid = s["id"].as_str().unwrap().to_owned();
    let ev = turns(sid.clone(), 1).await;
    let outs: Vec<String> =
        ev.iter().filter(|e| e["type"] == "tool_result").map(|e| e["output"].as_str().unwrap_or("").to_owned()).collect();
    assert!(outs[0].contains("projects.work") && outs[0].contains("free"), "{}", outs[0]);
    let all = get("work".into()).await;
    let made = all.as_array().unwrap().iter().find(|w| w["title"] == "Harden retries").unwrap_or_else(|| panic!("{all}; {outs:?}")).clone();
    let mid = made["id"].as_str().unwrap().to_owned();
    assert_eq!(made["project"], pid.as_str(), "{made}");
    let rec = get(format!("collections/work/records/{mid}")).await;
    assert_eq!(rec["created_by"], format!("session:{sid}"), "the session made it: {rec}");
    assert_eq!(made["made_by"], sid.as_str(), "the work item says which session made it");
    assert!(get(format!("projects/{pid}")).await.get("made_by").is_none(), "you made the project");

    // 2. To-dos on the work it made, and renaming it: free.
    mock.push(Reply::ToolCalls(vec![
        ("carbon6".into(), json!({"op": "work.todos.add", "input": {"id": mid, "body": {"text": "Add backoff"}}}).to_string()),
        ("carbon6".into(), json!({"op": "work.todos.add", "input": {"id": mid, "body": {"text": "Test a partition"}}}).to_string()),
        ("carbon6".into(), json!({"op": "work.edit", "input": {"id": mid, "body": {"title": "Harden retries and timeouts"}}}).to_string()),
    ]));
    mock.push(Reply::Text("Added.".into()));
    send(M::POST, format!("sessions/{sid}/messages"), json!({"text": "capture these as to-dos"})).await;
    turns(sid.clone(), 2).await;
    let m = get(format!("work/{mid}")).await;
    let texts: Vec<&str> = m["todos"].as_array().unwrap().iter().filter_map(|t| t["text"].as_str()).collect();
    assert_eq!(texts, ["Add backoff", "Test a partition"], "{m}");
    assert_eq!(m["title"], "Harden retries and timeouts");

    // 3. Renaming your project asks you once; nothing changes until you allow it.
    let rename = json!({"op": "projects.edit", "input": {"id": pid, "body": {"title": "Falcon"}}});
    mock.push(tool(rename.clone()));
    mock.push(Reply::Text("Asked.".into()));
    send(M::POST, format!("sessions/{sid}/messages"), json!({"text": "rename the project to Falcon"})).await;
    let ev = turns(sid.clone(), 3).await;
    assert!(last_result(&ev).contains("Not yet"), "{}", last_result(&ev));
    assert_eq!(get(format!("projects/{pid}")).await["title"], "Falcon Robustness");
    let ds = get("decisions".into()).await;
    let d = ds.as_array().unwrap().iter().find(|d| d["title"].as_str().unwrap_or("").contains("Falcon Robustness")).unwrap().clone();
    // Your answer reaches the session as a message (a turn), and it calls again.
    mock.push(tool(rename.clone()));
    mock.push(Reply::Text("Renamed.".into()));
    let r = send(M::POST, format!("decisions/{}/answer", d["id"]), json!({"option": "Allow"})).await;
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let ev = turns(sid.clone(), 4).await;
    assert!(
        ev.iter().any(|e| e["type"] == "user" && e["text"].as_str().unwrap_or("").contains("The user allowed decision")),
        "the session heard the answer"
    );
    assert_eq!(get(format!("projects/{pid}")).await["title"], "Falcon");

    // 4. Removing your to-do asks each time, and Allow does it.
    mock.push(tool(json!({"op": "work.todos.remove", "input": {"id": wid, "todo": your_todo}})));
    mock.push(Reply::Text("Asked to remove it.".into()));
    send(M::POST, format!("sessions/{sid}/messages"), json!({"text": "remove my to-do"})).await;
    let ev = turns(sid.clone(), 5).await;
    assert!(last_result(&ev).contains("Not yet"), "{}", last_result(&ev));
    assert_eq!(get(format!("work/{wid}")).await["todos"].as_array().unwrap().len(), 1);
    let ds = get("decisions".into()).await;
    let d = ds
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["title"].as_str().unwrap_or("").contains("Your work"))
        .unwrap_or_else(|| panic!("{ds}"))
        .clone();
    mock.push(Reply::Text("Gone.".into()));
    let r = send(M::POST, format!("decisions/{}/answer", d["id"]), json!({"option": "Allow"})).await;
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    turns(sid.clone(), 6).await;
    assert!(get(format!("work/{wid}")).await["todos"].as_array().is_none_or(Vec::is_empty), "removed once you allowed it");

    // 5. Administration is yours alone; so is another realm.
    let templates_before = get("settings/templates".into()).await;
    let policies_before = get("settings/policies".into()).await;
    mock.push(Reply::ToolCalls(vec![
        (
            "carbon6".into(),
            json!({"op": "connections.create", "input": {"body": {"name": "x", "base_url": "http://127.0.0.1:1"}}}).to_string(),
        ),
        ("carbon6".into(), json!({"op": "settings.templates.put", "input": {"realm": "home", "body": []}}).to_string()),
        ("carbon6".into(), json!({"op": "settings.policies.put", "input": {"realm": "root", "body": []}}).to_string()),
        ("carbon6".into(), json!({"op": "work.add", "input": {"body": {"title": "Elsewhere", "realm": "commons"}}}).to_string()),
    ]));
    mock.push(Reply::Text("Couldn't.".into()));
    send(M::POST, format!("sessions/{sid}/messages"), json!({"text": "add a connection"})).await;
    let ev = turns(sid.clone(), 7).await;
    let outs: Vec<String> =
        ev.iter().filter(|e| e["type"] == "tool_result").rev().take(4).map(|e| e["output"].as_str().unwrap_or("").to_owned()).collect();
    assert_eq!(outs.iter().filter(|o| o.contains("the user's alone")).count(), 3, "{outs:?}");
    assert!(outs.iter().any(|o| o.contains("not commons")), "{outs:?}");
    assert_eq!(get("settings/templates".into()).await, templates_before);
    assert_eq!(get("settings/policies".into()).await, policies_before);
    let cs = get("connections".into()).await;
    assert_eq!(cs.as_array().unwrap().len(), 1);

    // 6. The work tool does the same: new work in a project named as you'd
    // say it, then a to-do on it by its id.
    let create = json!({"action": "create", "title": "Chaos drills", "done_when": "a drill a week", "project": "falcon"});
    mock.push(Reply::ToolCalls(vec![("work".into(), create.to_string())]));
    mock.push(Reply::Text("Made.".into()));
    send(M::POST, format!("sessions/{sid}/messages"), json!({"text": "new work for drills"})).await;
    let ev = turns(sid.clone(), 8).await;
    let made: Value = serde_json::from_str(&last_result(&ev)).unwrap_or_else(|_| panic!("{}", last_result(&ev)));
    let did = made["work"].as_str().unwrap().to_owned();
    assert_eq!(made["project"], pid.as_str());
    mock.push(Reply::ToolCalls(vec![("work".into(), json!({"action": "todo", "work": did, "text": "Schedule the first"}).to_string())]));
    mock.push(Reply::Text("Noted.".into()));
    send(M::POST, format!("sessions/{sid}/messages"), json!({"text": "and a to-do"})).await;
    turns(sid.clone(), 9).await;
    let d = get(format!("work/{did}")).await;
    assert_eq!(d["todos"][0]["text"], "Schedule the first", "{d}");
    assert_eq!(d["todos"][0]["origin"], sid.as_str(), "the to-do says which session added it");
    // Its words and prompt change in place (no copy, nothing ticked), and
    // one no longer wanted comes off without saying it was done.
    let tid = d["todos"][0]["id"].as_str().unwrap().to_owned();
    let edit = json!({"action": "todo_edit", "work": did, "id": tid, "text": "Schedule the first drill", "prompt": "Red: … Green: …"});
    mock.push(Reply::ToolCalls(vec![("work".into(), edit.to_string())]));
    mock.push(Reply::Text("Changed.".into()));
    send(M::POST, format!("sessions/{sid}/messages"), json!({"text": "add a test plan to it"})).await;
    turns(sid.clone(), 10).await;
    let d = get(format!("work/{did}")).await;
    let todos = d["todos"].as_array().unwrap();
    assert_eq!(todos.len(), 1, "changed in place: {d}");
    assert_eq!((todos[0]["text"].as_str(), todos[0]["prompt"].as_str()), (Some("Schedule the first drill"), Some("Red: … Green: …")));
    assert_ne!(todos[0]["done"], true);
    mock.push(Reply::ToolCalls(vec![("work".into(), json!({"action": "todo_remove", "work": did, "id": tid}).to_string())]));
    mock.push(Reply::Text("Removed.".into()));
    send(M::POST, format!("sessions/{sid}/messages"), json!({"text": "drop it"})).await;
    turns(sid.clone(), 11).await;
    let d = get(format!("work/{did}")).await;
    assert!(d["todos"].as_array().is_none_or(Vec::is_empty), "removed, not ticked: {d}");

    // 7. Your mail only within the session's knowledge scope, and sending in
    // your name asks each time.
    let threads = json!({"op": "mail.threads", "input": {}});
    mock.push(tool(threads.clone()));
    mock.push(Reply::Text("Can't.".into()));
    send(M::POST, format!("sessions/{sid}/messages"), json!({"text": "read my mail"})).await;
    let ev = turns(sid.clone(), 12).await;
    assert!(last_result(&ev).contains("knowledge scope to include email"), "{}", last_result(&ev));
    let r = send(M::PUT, "settings/memex_scopes".into(), json!([{"name": "mail", "kinds": ["email"]}])).await;
    assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    let mail = json!({"op": "mail.send", "input": {"to": ["a@example.com"], "subject": "Hi", "body": "Hello"}});
    mock.push(Reply::ToolCalls(vec![("carbon6".into(), threads.to_string()), ("carbon6".into(), mail.to_string())]));
    mock.push(Reply::Text("Asked.".into()));
    send(M::POST, format!("sessions/{sid}/messages"), json!({"text": "read my mail and send a note"})).await;
    let ev = turns(sid.clone(), 13).await;
    let outs: Vec<String> =
        ev.iter().filter(|e| e["type"] == "tool_result").rev().take(2).map(|e| e["output"].as_str().unwrap_or("").to_owned()).collect();
    assert!(outs.iter().all(|o| !o.contains("knowledge scope")), "within its scope now: {outs:?}");
    assert!(outs.iter().any(|o| o.contains("Not yet") && o.contains("in the user's name")), "{outs:?}");
    let ds = get("decisions".into()).await;
    assert!(
        ds.as_array().unwrap().iter().any(|d| d["document"].as_str().unwrap_or("").contains("mail.send")),
        "a card for this one send: {ds}"
    );
    stop.cancel();
}
