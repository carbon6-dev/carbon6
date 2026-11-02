//! Catching up on a session, and its timeline: the card and folded turns
//! off until a session's own switch (or its realm's default for new
//! sessions) turns them on; what it did since you looked (or your last
//! message) counted, and summed up by the model a `task:catchup` rule
//! picks, else the session's own; its turns, whatever its switches say,
//! each with a line from `task:timeline`'s model, else the session's own,
//! made once and kept.

use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, enroll};
use reqwest::{Method as M, StatusCode};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn catching_up_on_a_session_and_its_timeline() {
    let mock = MockLlm::start().await;
    let (head_dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
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
    let worker = crate::open_worker(state.path()).await;
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(c6_worker::conn::run(worker.clone(), tx, stop.clone()));
    c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    let wid = worker.identity.meta.worker_id.0;
    let send = |method: M, path: String, token: Option<String>, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move {
            let mut r = http.request(method.clone(), url);
            if method != M::GET {
                r = r.json(&body);
            }
            if let Some(t) = token {
                r = r.bearer_auth(t);
            }
            r.send().await.unwrap()
        }
    };
    let ok = async |r: reqwest::Response| -> Value {
        let status = r.status();
        let text = r.text().await.unwrap();
        assert!(status.is_success(), "{status}: {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    };
    let get = async |path: String| -> Value { ok(send(M::GET, path, None, json!(null)).await).await };
    let conn = ok(send(M::POST, "connections".into(), None, json!({"name": "m", "base_url": mock.base_url()})).await).await;
    let ended = async |id: &str, turns: usize| {
        eventually("the turn to end", async || {
            let d: Value = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
            let ev: Vec<Value> = http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok()?;
            let n = ev.iter().filter(|e| e["type"] == "turn_ended").count();
            (d["session"]["status"] == "idle" && n >= turns).then_some(ev)
        })
        .await
    };
    let start = async |prompt: &str| -> String {
        let body = json!({"worker_id": wid, "cwd": project.path().display().to_string(), "connection_id": conn["id"], "model": "m", "prompt": prompt});
        ok(send(M::POST, "sessions".into(), None, body).await).await["id"].as_str().unwrap().to_owned()
    };
    // Five commands, each a step, then the reply: six steps. Their output
    // never reaches the catch-up's model.
    let five = || {
        mock.push(Reply::ToolCalls(
            (1..=3).map(|i| ("bash".to_owned(), json!({"command": format!("printf 'OUT%s' PUT{i}")}).to_string())).collect(),
        ));
        mock.push(Reply::ToolCalls(
            (4..=5).map(|i| ("bash".to_owned(), json!({"command": format!("printf 'OUT%s' PUT{i}")}).to_string())).collect(),
        ));
        mock.push(Reply::Text("Ran all five.".into()));
    };

    // 1. Off by default: no card, no folding; its turns all the same (the
    // glance's outline is always there).
    mock.push(Reply::Text("Hi.".into()));
    let a = start("hello").await;
    ended(&a, 1).await;
    assert_eq!(get(format!("sessions/{a}/view")).await, json!({"catchup": false, "fold": false}));
    assert_eq!(send(M::GET, format!("sessions/{a}/catchup"), None, json!(null)).await.status(), StatusCode::NO_CONTENT);
    let turns = get(format!("sessions/{a}/timeline")).await;
    assert_eq!(
        (turns[0]["by"].clone(), turns[0]["words"].clone(), turns[0]["reply"].clone()),
        (json!("you"), json!("hello"), json!("Hi.")),
        "{turns}"
    );

    // 2. The realm's defaults turn them on for new sessions (not for one
    // already started); catch-ups go to a small model.
    ok(send(M::PUT, "settings/suggestions".into(), None, json!({"enabled": true, "catchup": true, "fold": true})).await).await;
    let s = get("settings/suggestions".into()).await;
    assert_eq!((s["catchup"].clone(), s["fold"].clone()), (json!(true), json!(true)), "{s}");
    ok(send(
        M::PUT,
        "settings/routes".into(),
        None,
        json!([{"name": "catch-up", "applies_to": ["task:catchup"], "connection": conn["id"], "model": "small", "enabled": true}]),
    )
    .await)
    .await;
    five();
    let b = start("run the five commands").await;
    let events = ended(&b, 1).await;
    assert_eq!(get(format!("sessions/{b}/view")).await, json!({"catchup": true, "fold": true}));
    assert_eq!(get(format!("sessions/{a}/view")).await, json!({"catchup": false, "fold": false}));

    // 3. Never looked at: since your message, six steps, worth a card,
    // counted; no paragraph until asked.
    let prompt_seq = events.iter().find(|e| e["type"] == "user").unwrap()["seq"].as_u64().unwrap();
    let c = get(format!("sessions/{b}/catchup")).await;
    assert_eq!(c["since_seq"], prompt_seq, "{c}");
    assert_eq!((c["steps"].clone(), c["offer"].clone(), c["summary"].clone()), (json!(6), json!(true), json!("")), "{c}");
    let commands = c["rows"].as_array().unwrap().iter().find(|r| r["kind"] == "commands").cloned().unwrap_or_default();
    assert_eq!(commands["text"], "5 commands run", "{c}");
    assert_eq!(commands["items"].as_array().map(Vec::len), Some(5), "{c}");

    // 4. The paragraph: from the task:catchup rule's model, with tool calls
    // but never their output; made once.
    mock.push(Reply::Text("It ran the five commands and said so.".into()));
    let c = get(format!("sessions/{b}/catchup?summary=true")).await;
    assert_eq!(c["summary"], "It ran the five commands and said so.", "{c}");
    let asked = mock.requests().last().unwrap().clone();
    assert_eq!(asked["model"], "small", "the routing rule's model");
    let text = asked["messages"].to_string();
    assert!(text.contains("printf") && text.contains("run the five commands") && text.contains("Ran all five."), "{text}");
    assert!(!text.contains("OUTPUT"), "no tool output: {text}");
    let calls = mock.requests().len();
    assert_eq!(get(format!("sessions/{b}/catchup?summary=true")).await["summary"], c["summary"], "cached");
    assert_eq!(mock.requests().len(), calls, "no second call for the same events");

    // 5. Seen: nothing new since you looked.
    ok(send(M::POST, format!("sessions/{b}/seen"), None, json!(null)).await).await;
    let c = get(format!("sessions/{b}/catchup")).await;
    assert_eq!((c["since_seq"].clone(), c["steps"].clone(), c["offer"].clone()), (c["last_seq"].clone(), json!(0), json!(false)), "{c}");

    // 6. Your message is later than what you saw; a reply alone is under
    // the threshold.
    mock.push(Reply::Text("Done again.".into()));
    ok(send(M::POST, format!("sessions/{b}/messages"), None, json!({"text": "and again"})).await).await;
    let events = ended(&b, 2).await;
    let again = events.iter().rfind(|e| e["type"] == "user").unwrap()["seq"].as_u64().unwrap();
    let c = get(format!("sessions/{b}/catchup?summary=true")).await;
    assert_eq!((c["since_seq"].clone(), c["steps"].clone(), c["offer"].clone()), (json!(again), json!(1), json!(false)), "{c}");
    assert_eq!(c["summary"], "", "no paragraph for what isn't worth a card");
    assert_eq!(mock.requests().len(), calls + 1, "and no call");
    // From an event, as an earlier answer said.
    assert_eq!(get(format!("sessions/{b}/catchup?since={prompt_seq}")).await["steps"], 7);

    // 7. The timeline: its turns, who started them and their steps; an
    // hour's lines from the session's own model (no task:timeline rule),
    // made once.
    let turns = get(format!("sessions/{b}/timeline")).await;
    let turns = turns.as_array().unwrap().clone();
    assert_eq!(turns.len(), 2, "{turns:?}");
    assert_eq!(
        (turns[0]["by"].clone(), turns[0]["words"].clone(), turns[0]["steps"].clone()),
        (json!("you"), json!("run the five commands"), json!(6))
    );
    assert_eq!(
        (turns[1]["words"].clone(), turns[1]["steps"].clone(), turns[1]["reply"].clone()),
        (json!("and again"), json!(1), json!("Done again."))
    );
    assert!(turns[0].get("summary").is_none(), "no line until its hour is opened");
    let at = turns[0]["at"].as_i64().unwrap();
    let hour = at - at.rem_euclid(3_600_000);
    let n = |i: usize| turns[i]["turn"].as_u64().unwrap();
    mock.push(Reply::Text(format!("{}: Ran five commands\n{}: Did it again", n(0), n(1))));
    let got = get(format!("sessions/{b}/timeline?hour={hour}")).await;
    assert_eq!(got[0]["summary"], "Ran five commands", "{got}");
    assert_eq!(mock.requests().last().unwrap()["model"], "m", "the session's own model");
    let calls = mock.requests().len();
    assert_eq!(get(format!("sessions/{b}/timeline")).await[0]["summary"], "Ran five commands", "kept");
    get(format!("sessions/{b}/timeline?hour={hour}")).await;
    assert_eq!(mock.requests().len(), calls, "no second call for the same turns");

    // 8. Routed the other way: catch-ups on the session's own model, the
    // timeline's lines on task:timeline's.
    ok(send(
        M::PUT,
        "settings/routes".into(),
        None,
        json!([{"name": "timeline", "applies_to": ["task:timeline"], "connection": conn["id"], "model": "tiny", "enabled": true}]),
    )
    .await)
    .await;
    five();
    ok(send(M::POST, format!("sessions/{b}/messages"), None, json!({"text": "once more"})).await).await;
    ended(&b, 3).await;
    mock.push(Reply::Text("Five more.".into()));
    assert_eq!(get(format!("sessions/{b}/catchup?summary=true")).await["summary"], "Five more.");
    assert_eq!(mock.requests().last().unwrap()["model"], "m", "no task:catchup rule: the session's own");
    let third = get(format!("sessions/{b}/timeline")).await.as_array().unwrap().last().cloned().unwrap();
    let at = third["at"].as_i64().unwrap();
    mock.push(Reply::Text(format!("{}: Ran five more", third["turn"])));
    let got = get(format!("sessions/{b}/timeline?hour={}", at - at.rem_euclid(3_600_000))).await;
    assert_eq!(got.as_array().unwrap().last().unwrap()["summary"], "Ran five more", "{got}");
    assert_eq!(mock.requests().last().unwrap()["model"], "tiny", "the task:timeline rule's model");

    // 9. A session's own switches, and only for its realm.
    ok(send(M::PUT, format!("sessions/{a}/view"), None, json!({"catchup": true, "fold": false})).await).await;
    assert_eq!(get(format!("sessions/{a}/view")).await, json!({"catchup": true, "fold": false}));
    // The old switch's name reads as folding.
    ok(send(M::PUT, format!("sessions/{a}/view"), None, json!({"catchup": true, "timeline": true})).await).await;
    assert_eq!(get(format!("sessions/{a}/view")).await, json!({"catchup": true, "fold": true}));
    assert_eq!(get(format!("sessions/{a}/catchup")).await["offer"], false, "one step");
    ok(send(M::POST, "realms".into(), None, json!({"id": "lab"})).await).await;
    let token = ok(send(M::POST, "auth/tokens".into(), None, json!({"name": "lab", "realm": "lab"})).await).await;
    let lab = token["token"].as_str().unwrap().to_owned();
    for path in [format!("sessions/{b}/catchup"), format!("sessions/{b}/timeline"), format!("sessions/{b}/view")] {
        assert_eq!(send(M::GET, path.clone(), Some(lab.clone()), json!(null)).await.status(), StatusCode::NOT_FOUND, "{path} as lab");
    }

    // 10. A long Claude Code session (thousands of events), folding
    // switched off (the realm's default turned it on): its timeline has
    // every turn, however it started (your message, a task's
    // notification, another session's, or the CLI by itself), the last one
    // still running.
    let mut long = head.state.store.session(&a).await.unwrap().unwrap();
    let cc_worker = c6_core::WorkerId(uuid::Uuid::now_v7());
    (long.id, long.worker_id) = ("long-claude-code".into(), cc_worker.to_string());
    head.state.store.create_session(&long, &[], false).await.unwrap();
    let events = claude_code_log(267);
    assert!(events.len() > 7_000, "{}", events.len());
    for batch in events.chunks(500) {
        head.state.store.ingest(cc_worker, &long.id, batch).await.unwrap().unwrap();
    }
    ok(send(M::PUT, format!("sessions/{}/view", long.id), None, json!({"catchup": false, "fold": false})).await).await;
    let turns = get(format!("sessions/{}/timeline", long.id)).await;
    let turns = turns.as_array().unwrap();
    assert_eq!(turns.len(), 267, "every turn");
    assert_eq!(turns.iter().map(|t| t["turn"].as_u64().unwrap()).collect::<Vec<_>>(), (1..=267).collect::<Vec<_>>());
    let by = |b: &str| turns.iter().filter(|t| t["by"] == b).count();
    assert_eq!((by("you"), by("task"), by("session"), by("")), (134, 45, 44, 44), "who started them");
    assert!(turns.iter().all(|t| t["steps"].as_u64() == Some(13)), "{:?}", turns[0]);
    assert_eq!(turns[0]["words"], "turn 1, please");
    assert_eq!(turns[0]["reply"], "Done with turn 1.");
    assert!(turns[..266].iter().all(|t| t["end_seq"].as_u64() > Some(0)), "ended");
    assert_eq!(turns[266]["end_seq"], 0, "the last one runs");

    stop.cancel();
    head.shutdown().await.unwrap();
}

/// A Claude Code session's log as its worker journals it, `turns` turns
/// long (the last still running), each of 13 steps: six calls of two
/// commands, then the reply. Its turns start, in turn, on your message, a
/// background task's notification, your message, another session's
/// message, your message, and on nothing said (the CLI by itself); each
/// after a checkpoint.
fn claude_code_log(turns: u64) -> Vec<c6_proto::SessionEvent> {
    use c6_proto::{
        AssistantMessage, BackgroundTask, Checkpoint, ContentBlock, InputDelivered, ModelSpec, NoticeQueued, SessionEvent, SessionStarted,
        ToolResult, ToolUse, TurnEndReason, TurnEnded, TurnStarted, Usage, UserMessage, content_block::Block, session_event::Kind,
    };
    let mut out = Vec::new();
    let mut push = |kind: Kind| {
        let seq = out.len() as u64 + 1;
        out.push(SessionEvent { seq, ts_ms: 1_793_412_506_000 + seq as i64 * 10_000, kind: Some(kind) });
    };
    let model = ModelSpec { connection_id: "cc".into(), model: "opus".into(), connection_kind: "claude_code".into(), ..Default::default() };
    push(Kind::Started(SessionStarted {
        command_id: "c0".into(),
        model: Some(model),
        tool_profile: "claude_code".into(),
        ..Default::default()
    }));
    let usage = || {
        Kind::Usage(Usage {
            model: "claude-opus".into(),
            input_tokens: 4,
            output_tokens: 7,
            context_window: 1_000_000,
            ..Default::default()
        })
    };
    for turn in 1..=turns {
        let id = format!("c{turn}");
        let user = |text: String| UserMessage { command_id: id.clone(), text, ..Default::default() };
        let checkpoint = Kind::Checkpoint(Checkpoint { commit: format!("{turn:040x}"), ..Default::default() });
        match turn % 6 {
            2 => {
                let task = BackgroundTask {
                    task_id: format!("t{turn}"),
                    kind: "claude_code".into(),
                    description: "npm test".into(),
                    state: "finished".into(),
                    ..Default::default()
                };
                push(Kind::BackgroundTask(task));
                push(checkpoint);
                let text = format!("<task-notification>\n<task-id>t{turn}</task-id>\nTask: npm test\n</task-notification>");
                push(Kind::User(UserMessage { command: "task-notification".into(), ..user(text) }));
            }
            4 => {
                let (from, title) = ("01a195b3".to_owned(), "Release prep".to_owned());
                push(Kind::NoticeQueued(NoticeQueued {
                    command_id: id.clone(),
                    text: "Is it in?".into(),
                    from_session_id: from.clone(),
                    from_title: title.clone(),
                    ..Default::default()
                }));
                push(checkpoint);
                push(Kind::User(UserMessage {
                    command: "session-message".into(),
                    from_session_id: from,
                    from_title: title,
                    ..user("Is it in?".into())
                }));
            }
            0 => push(checkpoint),
            _ => {
                push(checkpoint);
                push(Kind::User(user(format!("turn {turn}, please"))));
            }
        }
        push(Kind::TurnStarted(TurnStarted { turn, context_window: 1_000_000 }));
        for call in 0..6 {
            let calls: Vec<String> = (0..2).map(|i| format!("toolu_{turn}_{call}_{i}")).collect();
            let blocks = calls
                .iter()
                .map(|c| ContentBlock {
                    block: Some(Block::ToolUse(ToolUse {
                        id: c.clone(),
                        name: "Bash".into(),
                        input_json: r#"{"command":"git status"}"#.into(),
                    })),
                })
                .collect();
            push(Kind::Assistant(AssistantMessage { turn, blocks, ..Default::default() }));
            push(usage());
            for c in calls {
                push(Kind::ToolResult(ToolResult { call_id: c, tool: "Bash".into(), output: "clean".into(), ..Default::default() }));
            }
            if call == 2 {
                push(Kind::InputDelivered(InputDelivered { command_id: id.clone() }));
            }
        }
        let reply = ContentBlock { block: Some(Block::Text(format!("Done with turn {turn}. Nothing else to do."))) };
        push(Kind::Assistant(AssistantMessage { turn, blocks: vec![reply], ..Default::default() }));
        push(usage());
        if turn < turns {
            push(Kind::TurnEnded(TurnEnded { turn, reason: TurnEndReason::Completed as i32 }));
        }
    }
    out
}
