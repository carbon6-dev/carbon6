//! Task cards (docs/task-cards.md): an agent proposes work for a separate
//! session; the card shows in its session and in Decisions, and "Start it"
//! starts that session where the first one worked, with the card's prompt.

use c6_api::work::{TodoStart, WorkItem};
use c6_api::{Connection, Decision, Event, EventKind, Session};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use prost::Message;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn agents_propose_tasks_you_start_with_one_click() {
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
    let get = |path: String| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.get(url).send().await.unwrap().json::<Value>().await.unwrap() }
    };
    let start = |prompt: &'static str| {
        let (http, api, conn, cwd) = (http.clone(), api.clone(), conn.id.clone(), project.path().to_owned());
        async move {
            http.post(format!("{api}/sessions"))
                .json(&json!({"worker_id": wid, "cwd": cwd, "connection_id": conn, "model": "m", "prompt": prompt}))
                .send()
                .await
                .unwrap()
                .json::<Session>()
                .await
                .unwrap()
        }
    };
    let ended = |id: String| async move {
        eventually("the turn to end", async || {
            let ev: Vec<Event> = serde_json::from_value(get(format!("sessions/{id}/events")).await).ok()?;
            ev.iter().any(|e| matches!(e.kind, EventKind::TurnEnded { .. })).then_some(ev)
        })
        .await
    };

    // 1. While fixing a test, the agent proposes a cleanup (twice: one card).
    let card = json!({"title": "Remove the dead retry module", "why": "src/retry.rs is never called since the client retries itself.",
                      "prompt": "Delete src/retry.rs and its tests; nothing calls it (grep retry::). Run the tests."})
    .to_string();
    mock.push(Reply::ToolCalls(vec![("task_propose".into(), card.clone())]));
    mock.push(Reply::ToolCalls(vec![("task_propose".into(), card)]));
    mock.push(Reply::Text("Fixed the test; I proposed removing the dead retry module separately.".into()));
    let s1 = start("fix the flaky test").await;
    let events = ended(s1.id.clone()).await;
    let EventKind::Started { tools, system_prompt, .. } = &events[0].kind else { panic!("{:?}", events[0]) };
    assert!(tools.iter().any(|t| t == "task_propose") && tools.iter().any(|t| t == "task_withdraw"), "{tools:?}");
    assert!(system_prompt.contains("# Task cards") && system_prompt.contains("dead code"), "the guidance is in its instructions");
    let replies = mock.requests()[2]["messages"].to_string();
    assert!(replies.contains("task card #1 proposed") && replies.contains("already open"), "{replies}");

    let cards: Vec<Decision> = serde_json::from_value(get(format!("sessions/{}/decisions", s1.id)).await).unwrap();
    assert_eq!(cards.len(), 1, "{cards:?}");
    let c = &cards[0];
    assert_eq!((c.kind.as_str(), c.priority.as_str(), c.group.as_str(), c.waiting, c.count), ("task", "low", "Suggested tasks", false, 2));
    assert_eq!(c.title, "Remove the dead retry module");
    assert!(c.document.contains("never called"));
    let listed: Vec<Decision> = serde_json::from_value(get("decisions".into()).await).unwrap();
    assert!(listed.iter().any(|d| d.id == c.id), "in Decisions too");

    // 2. Start it: a new session does it, where the first one worked.
    mock.push(Reply::Text("Removed it; the tests pass.".into()));
    let r = http.post(format!("{api}/decisions/{}/answer", c.id)).json(&json!({"option": "Start it"})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    let done: Decision = serde_json::from_value(get(format!("decisions/{}", c.id)).await).unwrap();
    assert_eq!((done.state.as_str(), done.sessions.len()), ("answered", 1), "{done:?}");
    let task = done.sessions[0].clone();
    ended(task.clone()).await;
    let detail = get(format!("sessions/{task}")).await;
    let s = &detail["session"];
    assert_eq!(s["title"], "Remove the dead retry module", "{s}");
    assert_eq!(s["cwd"].as_str().map(std::path::Path::new), Some(project.path().canonicalize().unwrap().as_path()), "{s}");
    let labels = s["labels"].to_string();
    assert!(labels.contains("task-card") && labels.contains(&format!("from:{}", s1.id)), "{labels}");
    let asked = mock.requests().last().unwrap()["messages"].to_string();
    assert!(asked.contains("Delete src/retry.rs and its tests"), "its prompt is the card's: {asked}");

    // 3. Done after all: the agent withdraws its card.
    mock.push(Reply::ToolCalls(vec![(
        "task_propose".into(),
        json!({"title": "Document the cache flags", "why": "README lacks them.", "prompt": "Add the cache flags to README."}).to_string(),
    )]));
    mock.push(Reply::ToolCalls(vec![("task_withdraw".into(), json!({"id": 2, "why": "I documented them myself."}).to_string())]));
    mock.push(Reply::Text("Documented them.".into()));
    let s2 = start("tidy the README").await;
    ended(s2.id.clone()).await;
    let gone: Decision = serde_json::from_value(get("decisions/2".into()).await).unwrap();
    assert_eq!((gone.title.as_str(), gone.state.as_str()), ("Document the cache flags", "withdrawn"), "{gone:?}");
    let left: Vec<Decision> = serde_json::from_value(get(format!("sessions/{}/decisions", s2.id)).await).unwrap();
    assert!(left.is_empty(), "{left:?}");

    // 4. The other answers: do it in the proposing session, or keep it as a
    // to-do in that session's work or in new work. (No work notes: they'd
    // take the mock's replies.)
    let r = http.put(format!("{api}/settings/work")).json(&json!({"notes": false})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    let propose =
        |title: &str, prompt: &str| ("task_propose".to_owned(), json!({"title": title, "why": "w", "prompt": prompt}).to_string());
    mock.push(Reply::ToolCalls(vec![
        propose("Bump the lockfile", "Run cargo update -p serde."),
        propose("Split the config module", "Split src/config.rs into load.rs and check.rs."),
        propose("Fix the README typo", "README says 'recieve'; fix it."),
    ]));
    mock.push(Reply::Text("Sped up the build.".into()));
    let s3: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": wid, "cwd": project.path(), "connection_id": conn.id, "model": "m", "prompt": "speed up the build", "work": "new"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    ended(s3.id.clone()).await;
    let cards: Vec<Decision> = serde_json::from_value(get(format!("sessions/{}/decisions", s3.id)).await).unwrap();
    let card = |title: &str| cards.iter().find(|c| c.title == title).unwrap().clone();
    let labels: Vec<&str> = cards[0].options.iter().map(|o| o.label.as_str()).collect();
    assert_eq!(labels, ["Start it", "Do it here", "Add to this work", "Add as new work", "Not now"]);
    let answer = |id: i64, option: &'static str| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            let r = http.post(format!("{api}/decisions/{id}/answer")).json(&json!({"option": option})).send().await.unwrap();
            assert!(r.status().is_success(), "{option}: {}", r.text().await.unwrap());
        }
    };

    // Do it here: the session is told, and does it.
    mock.push(Reply::Text("Bumped it.".into()));
    answer(card("Bump the lockfile").id, "Do it here").await;
    eventually("the session to be told", async || {
        let m = mock.requests().last()?["messages"].to_string();
        (m.contains("Do the task you proposed here") && m.contains("cargo update -p serde")).then_some(())
    })
    .await;
    let here: Decision = serde_json::from_value(get(format!("decisions/{}", card("Bump the lockfile").id)).await).unwrap();
    assert!(here.sessions.is_empty(), "no new session: {here:?}");

    // Add to this work: a to-do in the work the session belongs to.
    let split = card("Split the config module");
    answer(split.id, "Add to this work").await;
    let of: Vec<WorkItem> =
        http.get(format!("{api}/work/of")).query(&[("ref", format!("session:{}", s3.id))]).send().await.unwrap().json().await.unwrap();
    assert_eq!(of.len(), 1, "{of:?}");
    let todo = of[0].fields.todos.iter().find(|t| t.text == "Split the config module").unwrap_or_else(|| panic!("{of:?}"));
    assert_eq!(
        (todo.prompt.as_str(), todo.from.clone(), todo.origin.as_str()),
        ("Split src/config.rs into load.rs and check.rs.", format!("decision:{}", split.id), s3.id.as_str())
    );
    assert!(todo.done_at.is_none());

    // Add as new work: work of its own, whose to-do starts from the
    // session that proposed it, with the card's prompt.
    let typo = card("Fix the README typo");
    answer(typo.id, "Add as new work").await;
    let all: Vec<WorkItem> = serde_json::from_value(get("work".into()).await).unwrap();
    let w = all.iter().find(|w| w.fields.title == "Fix the README typo").unwrap_or_else(|| panic!("{all:?}"));
    assert_eq!((w.state.as_str(), w.why.as_str()), ("open", "1 to-do left: Fix the README typo"), "{w:?}");
    let start: TodoStart = serde_json::from_value(get(format!("work/{}/todos/{}/start", w.id, w.fields.todos[0].id)).await).unwrap();
    assert!(start.text.contains("README says 'recieve'") && start.text.contains(&w.fields.todos[0].id), "{start:?}");
    assert_eq!(start.seed.as_deref(), Some(s3.id.as_str()));

    stop.cancel();
    head.shutdown().await.unwrap();
}

struct Cards {
    head: Option<c6_head::Head>,
    config: HeadConfig,
    _head_dir: tempfile::TempDir,
    state: tempfile::TempDir,
    project: tempfile::TempDir,
    worker: std::sync::Arc<Worker>,
    stop: CancellationToken,
    http: reqwest::Client,
    api: String,
    mock: MockLlm,
    connection: String,
}

impl Cards {
    async fn new() -> Self {
        let mock = MockLlm::start().await;
        let (head_dir, state, project) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let config = HeadConfig {
            data_dir: head_dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: c6_testkit::port(),
            worker_port: c6_testkit::port(),
            auth: false,
            ..Default::default()
        };
        let head = c6_head::start(config.clone()).await.unwrap();
        let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
        let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
        enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
        let worker = crate::open_worker(state.path()).await;
        let mut this = Self {
            head: Some(head),
            config,
            _head_dir: head_dir,
            state,
            project,
            worker,
            stop: CancellationToken::new(),
            http: reqwest::Client::new(),
            api,
            mock,
            connection: String::new(),
        };
        this.attach().await;
        let conn: Connection =
            serde_json::from_value(this.post("connections", json!({"name": "m", "base_url": this.mock.base_url()})).await).unwrap();
        this.connection = conn.id;
        let r = this.http.put(format!("{}/settings/work", this.api)).json(&json!({"notes": false})).send().await.unwrap();
        assert!(r.status().is_success());
        this
    }

    fn head(&self) -> &std::sync::Arc<c6_head::HeadState> {
        &self.head.as_ref().unwrap().state
    }

    async fn attach(&mut self) {
        self.stop = CancellationToken::new();
        let (tx, mut rx) = watch::channel(ConnState::Connecting);
        tokio::spawn(conn::run(self.worker.clone(), tx, self.stop.clone()));
        c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
    }

    async fn detach(&self) {
        self.stop.cancel();
        eventually("the worker to detach", async || self.head().registry.get(self.worker.identity.meta.worker_id).is_none().then_some(()))
            .await;
    }

    async fn post(&self, path: &str, body: Value) -> Value {
        let r = self.http.post(format!("{}/{path}", self.api)).json(&body).send().await.unwrap();
        let status = r.status();
        let text = r.text().await.unwrap();
        assert!(status.is_success(), "{path}: {status} {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    async fn get(&self, path: &str) -> Value {
        self.http.get(format!("{}/{path}", self.api)).send().await.unwrap().json().await.unwrap()
    }

    async fn events(&self, session: &str) -> Vec<Event> {
        serde_json::from_value(self.get(&format!("sessions/{session}/events")).await).unwrap()
    }

    async fn idle(&self, session: &str, turns: usize) {
        eventually("the session to finish its turn", async || {
            let d = self.get(&format!("sessions/{session}")).await;
            let ended = self.events(session).await.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count();
            (d["session"]["status"] == "idle" && ended == turns && d["queued"].as_array()?.is_empty()).then_some(())
        })
        .await;
    }

    async fn start(&self, work: bool) -> Session {
        self.mock.push(Reply::Text("Finished the original task.".into()));
        let session: Session = serde_json::from_value(
            self.post(
                "sessions",
                json!({
                    "worker_id": self.worker.identity.meta.worker_id.0, "cwd": self.project.path(),
                    "connection_id": self.connection, "model": "m", "prompt": "finish the original task",
                    "work": if work { Some("new") } else { None },
                }),
            )
            .await,
        )
        .unwrap();
        self.idle(&session.id, 1).await;
        session
    }

    async fn propose(&self, session: &str, title: &str) -> Decision {
        let card = c6_head::decisions::task_card(
            self.head(),
            session,
            "task_propose",
            &json!({
                "title": title, "why": "A separate cleanup", "prompt": "Clean up src/retry.rs and run the tests."
            }),
        )
        .await
        .unwrap();
        serde_json::from_value(self.get(&format!("decisions/{}", card["id"])).await).unwrap()
    }

    async fn recorded(&self, session: &str, text: &str) {
        eventually("the quiet note to be journaled", async || {
            let evs = self.worker.journal.events_after(session, 0, u32::MAX).await.ok()?;
            evs.iter()
                .any(|e| matches!(&e.kind, Some(c6_proto::session_event::Kind::NoticeQueued(n)) if n.quiet && !n.wake && n.text == text))
                .then_some(())
        })
        .await;
        eventually("the card answer to show once as your message without a turn", async || {
            let evs = self.events(session).await;
            (evs.iter()
                .filter(|e| matches!(&e.kind, EventKind::User { text: s, command, .. } if s == text && command == "card-answer"))
                .count()
                == 1)
                .then_some(())
        })
        .await;
    }

    async fn assert_quiet(&self, session: &str, turns: usize, requests: usize) {
        // A negative assertion: give any unintended automatic turn a chance
        // to show itself, after the note was explicitly recorded.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(self.mock.requests().len(), requests, "a quiet note never calls the model");
        let detail = self.get(&format!("sessions/{session}")).await;
        assert_eq!(detail["session"]["status"], "idle", "{detail}");
        assert!(detail["queued"].as_array().unwrap().is_empty(), "no queued-input count: {detail}");
        let evs = self.events(session).await;
        assert!(!evs.iter().any(|e| matches!(e.kind, EventKind::NoticeQueued { .. })), "no held-notice pill: {evs:?}");
        assert_eq!(evs.iter().filter(|e| matches!(e.kind, EventKind::TurnStarted { .. })).count(), turns);
    }

    async fn close(mut self) {
        self.stop.cancel();
        self.head.take().unwrap().shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn task_card_answers_are_quiet_context_and_withdraw_explains_them() {
    let t = Cards::new().await;
    let session = t.start(true).await;
    let other = t.start(false).await;
    let elsewhere: WorkItem = serde_json::from_value(t.post("work", json!({"title": "Other cleanup"})).await).unwrap();
    let mut turns = 1;
    for option in [
        "Not now".to_owned(),
        "Add to this work".into(),
        "Add as new work".into(),
        format!("Add to work:{}", elsewhere.id),
        "Start it".into(),
    ] {
        let title = if option == "Not now" { "Cleanup \"later\"\nsecond line".to_owned() } else { format!("Cleanup for {option}") };
        let card = t.propose(&session.id, &title).await;
        let requests = t.mock.requests().len() + usize::from(option == "Start it");
        if option == "Start it" {
            t.mock.push(Reply::Text("Started the separate task.".into()));
        }
        let mut live = t.head().hub.subscribe(&session.id);
        let answered = t.post(&format!("decisions/{}/answer", card.id), json!({"option": option})).await;
        let text = match option.as_str() {
            "Not now" => format!("Not now: task card #{} {:?}.", card.id, card.title),
            "Start it" => {
                t.idle(answered["sessions"][0].as_str().unwrap(), 1).await;
                format!("Start session: task card #{} {:?}.", card.id, card.title)
            }
            _ => {
                let works: Vec<WorkItem> = serde_json::from_value(t.get("work").await).unwrap();
                let work = works.iter().find(|w| w.fields.todos.iter().any(|todo| todo.from == format!("decision:{}", card.id))).unwrap();
                format!("Save for later: task card #{} {:?} (to-do on {:?}).", card.id, card.title, work.fields.title)
            }
        };
        assert_eq!(text.lines().count(), 1, "the note stays one line even for multiline titles");
        t.recorded(&session.id, &text).await;
        t.assert_quiet(&session.id, turns, requests).await;
        let shown = eventually("the card answer to stream as your message without a turn", async || {
            while let Ok(msg) = live.try_recv() {
                if let c6_api::StreamMsg::Event { event } = msg
                    && matches!(&event.kind, EventKind::User { text: s, command, .. } if s == &text && command == "card-answer")
                {
                    return Some(event);
                }
            }
            None
        })
        .await;
        assert!(t.events(&session.id).await.contains(&shown), "reload uses the same journal position as live view");
        // Redelivery before and after consumption must neither show nor send
        // another copy. The marker is in the existing command id, so even a
        // quiet_notes worker predating this UI behavior preserves it.
        let EventKind::User { command_id, .. } = &shown.kind else { unreachable!() };
        let command = c6_proto::SessionCommand {
            command_id: command_id.clone(),
            session_id: session.id.clone(),
            kind: Some(c6_proto::session_command::Kind::QuietNote(c6_proto::QuietNote { text: text.clone() })),
        };
        t.worker.sessions.handle(command.clone()).await.unwrap();
        let withdraw = |id| json!({"id": id, "why": "No longer needed"});
        let error = c6_head::decisions::task_card(t.head(), &session.id, "task_withdraw", &withdraw(card.id)).await.unwrap_err();
        assert_eq!(error.to_string(), format!("Task card #{} was already answered: {option}.", card.id));
        for (sid, id) in [(&other.id, card.id), (&session.id, i64::MAX)] {
            let error = c6_head::decisions::task_card(t.head(), sid, "task_withdraw", &withdraw(id)).await.unwrap_err();
            assert_eq!(error.to_string(), "no open task card of this session by that id");
        }
        t.mock.push(Reply::Text("I saw the card's answer.".into()));
        t.post(&format!("sessions/{}/messages", session.id), json!({"text": "continue"})).await;
        turns += 1;
        t.idle(&session.id, turns).await;
        let encoded = serde_json::to_string(&text).unwrap();
        assert_eq!(
            t.mock.requests().last().unwrap()["messages"].to_string().matches(&encoded[1..encoded.len() - 1]).count(),
            1,
            "{text}: {}",
            t.mock.log()
        );
        t.worker.sessions.handle(command).await.unwrap();
        t.assert_quiet(&session.id, turns, requests + 1).await;
        let journal = t.worker.journal.events_after(&session.id, 0, u32::MAX).await.unwrap();
        assert_eq!(
            journal
                .iter()
                .filter(|e| matches!(&e.kind, Some(c6_proto::session_event::Kind::NoticeQueued(n)) if n.command_id == *command_id))
                .count(),
            1
        );
        assert!(!std::iter::from_fn(|| live.try_recv().ok()).any(|msg| matches!(msg, c6_api::StreamMsg::Event { event } if matches!(&event.kind, EventKind::User { text: s, .. } if s == &text))), "no second live line when consumed or redelivered");
        let told = t.events(&session.id).await.iter().filter(|e| matches!(&e.kind, EventKind::User { text: s, .. } if s == &text)).count();
        assert_eq!(told, 1, "the note is delivered once");
    }

    // Save for later without session work creates a new destination, too.
    let card = t.propose(&other.id, "Standalone cleanup").await;
    t.post(&format!("decisions/{}/answer", card.id), json!({"option": "Add to this work"})).await;
    let text = format!("Save for later: task card #{} {:?} (to-do on {:?}).", card.id, card.title, card.title);
    t.recorded(&other.id, &text).await;
    t.assert_quiet(&other.id, 1, t.mock.requests().len()).await;
    t.mock.push(Reply::Text("Saved for later.".into()));
    t.post(&format!("sessions/{}/messages", other.id), json!({"text": "continue"})).await;
    t.idle(&other.id, 2).await;
    assert!(t.events(&other.id).await.iter().any(|e| matches!(&e.kind, EventKind::User { text: s, .. } if s == &text)));

    // An unsuccessful save leaves the card open, without announcing a save.
    let card = t.propose(&session.id, "An invalid destination").await;
    let before = t.worker.journal.events_after(&session.id, 0, u32::MAX).await.unwrap();
    let r = t
        .http
        .post(format!("{}/decisions/{}/answer", t.api, card.id))
        .json(&json!({"option": "Add to work:does-not-exist"}))
        .send()
        .await
        .unwrap();
    assert!(!r.status().is_success());
    assert_eq!(t.get(&format!("decisions/{}", card.id)).await["state"], "open");
    assert_eq!(t.worker.journal.events_after(&session.id, 0, u32::MAX).await.unwrap(), before);
    c6_head::decisions::task_card(t.head(), &session.id, "task_withdraw", &json!({"id": card.id, "why": "No destination"})).await.unwrap();
    t.close().await;
}

#[tokio::test]
async fn a_busy_task_card_note_waits_for_input_and_survives_restarts() {
    let mut t = Cards::new().await;
    let session = t.start(false).await;
    let card = t.propose(&session.id, "A busy cleanup").await;
    let gate = t.mock.push_gated(Reply::Text("The current turn is busy.".into()));
    t.post(&format!("sessions/{}/messages", session.id), json!({"text": "do the next step"})).await;
    eventually("the busy turn to reach the model and head", async || {
        (t.mock.requests().len() == 2 && t.get(&format!("sessions/{}", session.id)).await["session"]["status"] == "running").then_some(())
    })
    .await;
    t.post(&format!("decisions/{}/answer", card.id), json!({"option": "Not now"})).await;
    let text = format!("Not now: task card #{} {:?}.", card.id, card.title);
    t.recorded(&session.id, &text).await;
    assert_eq!(t.get(&format!("sessions/{}", session.id)).await["session"]["status"], "running");
    gate.notify_one();
    t.idle(&session.id, 2).await;
    t.assert_quiet(&session.id, 2, 2).await;
    assert!(!t.mock.requests()[1]["messages"].to_string().contains("A busy cleanup"), "not injected into the busy turn");

    // A queued command survives a head restart; a recorded note survives
    // a worker reload. Both show as your messages before the next input.
    t.detach().await;
    let offline = t.propose(&session.id, "An offline cleanup").await;
    t.post(&format!("decisions/{}/answer", offline.id), json!({"option": "Not now"})).await;
    let offline_text = format!("Not now: task card #{} {:?}.", offline.id, offline.title);
    t.head.take().unwrap().shutdown().await.unwrap();
    t.head = Some(c6_head::start(t.config.clone()).await.unwrap());
    // In-process shutdown leaves existing HTTP keep-alive tasks alive;
    // a real process restart closes those sockets.
    t.http = reqwest::Client::new();
    drop(t.worker);
    t.worker = crate::open_worker(t.state.path()).await;
    t.attach().await;
    t.recorded(&session.id, &text).await;
    t.recorded(&session.id, &offline_text).await;
    t.assert_quiet(&session.id, 2, 2).await;
    t.mock.push(Reply::Text("Both answers received.".into()));
    t.post(&format!("sessions/{}/messages", session.id), json!({"text": "continue after restart"})).await;
    t.idle(&session.id, 3).await;
    let events = t.events(&session.id).await;
    for expected in [&text, &offline_text] {
        assert_eq!(events.iter().filter(|e| matches!(&e.kind, EventKind::User { text, .. } if text == expected)).count(), 1);
    }
    let messages = t.mock.requests().last().unwrap()["messages"].to_string();
    assert_eq!(messages.matches("A busy cleanup").count(), 1, "{messages}");
    assert_eq!(messages.matches("An offline cleanup").count(), 1, "{messages}");
    // Reload after consumption, too: the raw User receipt still prevents
    // replaying the arrival, although the API no longer shows that receipt.
    t.detach().await;
    drop(t.worker);
    t.worker = crate::open_worker(t.state.path()).await;
    t.attach().await;
    t.recorded(&session.id, &text).await;
    t.recorded(&session.id, &offline_text).await;
    t.assert_quiet(&session.id, 3, 3).await;
    t.mock.push(Reply::Text("Both answers still appear only once.".into()));
    t.post(&format!("sessions/{}/messages", session.id), json!({"text": "continue after consumption and reload"})).await;
    t.idle(&session.id, 4).await;
    let messages = t.mock.requests().last().unwrap()["messages"].to_string();
    assert_eq!(messages.matches("A busy cleanup").count(), 1, "{messages}");
    assert_eq!(messages.matches("An offline cleanup").count(), 1, "{messages}");
    t.close().await;
}

#[tokio::test]
async fn a_task_card_note_restores_a_trimmed_session_without_waking() {
    let mut t = Cards::new().await;
    let session = t.start(false).await;
    t.detach().await;
    let journal = t.worker.journal.clone();
    let events = journal.events_after(&session.id, 0, u32::MAX).await.unwrap();
    journal.set_acked(&session.id, events.last().unwrap().seq).await.unwrap();
    drop(t.worker);
    assert_eq!(journal.trim(c6_core::now_ms() + 1000, &[]).await.unwrap(), 1);
    t.worker = crate::open_worker(t.state.path()).await;
    t.attach().await;
    assert!(!t.worker.journal.has_session(&session.id).await.unwrap());
    let card = t.propose(&session.id, "A cleanup after journal trimming").await;
    t.post(&format!("decisions/{}/answer", card.id), json!({"option": "Not now"})).await;
    let text = format!("Not now: task card #{} {:?}.", card.id, card.title);
    t.recorded(&session.id, &text).await;
    t.assert_quiet(&session.id, 1, 1).await;
    t.mock.push(Reply::Text("I saw the restored note.".into()));
    t.post(&format!("sessions/{}/messages", session.id), json!({"text": "continue after trimming"})).await;
    t.idle(&session.id, 2).await;
    assert_eq!(t.mock.requests().last().unwrap()["messages"].to_string().matches("A cleanup after journal trimming").count(), 1);
    assert_eq!(t.events(&session.id).await.iter().filter(|e| matches!(&e.kind, EventKind::User { text: s, .. } if s == &text)).count(), 1);

    // Other rejection reasons cannot turn quiet context into a failed
    // session (commands without queue text usually describe a Create).
    t.detach().await;
    let card = t.propose(&session.id, "A note the worker cannot record").await;
    t.post(&format!("decisions/{}/answer", card.id), json!({"option": "Not now"})).await;
    let pending = t.head().store.pending_commands(t.worker.identity.meta.worker_id, 0).await.unwrap();
    let is_note = |c: &c6_head::store::CommandRow| {
        matches!(c6_proto::SessionCommand::decode(c.data.as_slice()).unwrap().kind, Some(c6_proto::session_command::Kind::QuietNote(_)))
    };
    let note = pending.iter().find(|c| is_note(c)).unwrap();
    c6_head::sessions::on_rejected(
        t.head(),
        t.worker.identity.meta.worker_id,
        c6_proto::CommandRejected {
            command_id: note.id.clone(),
            session_id: session.id.clone(),
            reason: "could not record context".into(),
        },
    )
    .await
    .unwrap();
    assert!(t.head().store.pending_commands(t.worker.identity.meta.worker_id, 0).await.unwrap().iter().any(is_note));
    t.assert_quiet(&session.id, 2, 2).await;
    t.attach().await;
    let text = format!("Not now: task card #{} {:?}.", card.id, card.title);
    t.recorded(&session.id, &text).await;
    t.assert_quiet(&session.id, 2, 2).await;
    t.close().await;
}

#[tokio::test]
async fn old_workers_skip_task_card_notes_without_waking() {
    let t = Cards::new().await;
    let session = t.start(false).await;
    t.detach().await;
    let offline = t.propose(&session.id, "Queued before the old worker attaches").await;
    t.post(&format!("decisions/{}/answer", offline.id), json!({"option": "Not now"})).await;
    let identity = &t.worker.identity;
    let channel = tonic::transport::Endpoint::from_shared(identity.meta.head_url.clone())
        .unwrap()
        .tls_config(identity.tls_config())
        .unwrap()
        .connect()
        .await
        .unwrap();
    let (tx, rx) = tokio::sync::mpsc::channel(16);
    let events = t.worker.journal.events_after(&session.id, 0, u32::MAX).await.unwrap();
    tx.send(c6_proto::WorkerMsg {
        msg: Some(c6_proto::worker_msg::Msg::Hello(c6_proto::Hello {
            protocol_version: c6_core::PROTOCOL_VERSION,
            capabilities: vec!["queue_ops".into()],
            sessions: vec![c6_proto::SessionCursor { session_id: session.id.clone(), seq: events.last().unwrap().seq }],
            ..Default::default()
        })),
    })
    .await
    .unwrap();
    let mut inbound = c6_proto::worker_hub_client::WorkerHubClient::new(channel)
        .attach(tokio_stream::wrappers::ReceiverStream::new(rx))
        .await
        .unwrap()
        .into_inner();
    assert!(matches!(inbound.message().await.unwrap().unwrap().msg, Some(c6_proto::head_msg::Msg::Welcome(_))));
    eventually("the old worker's queued note to be skipped", async || {
        t.head().store.pending_commands(t.worker.identity.meta.worker_id, 0).await.ok()?.is_empty().then_some(())
    })
    .await;
    let card = t.propose(&session.id, "Already on an old worker").await;
    t.post(&format!("decisions/{}/answer", card.id), json!({"option": "Not now"})).await;
    let pending = t.head().store.pending_commands(t.worker.identity.meta.worker_id, 0).await.unwrap();
    assert!(pending.is_empty(), "the connected old worker's note was skipped at enqueue");
    tx.send(c6_proto::WorkerMsg { msg: Some(c6_proto::worker_msg::Msg::Ping(c6_proto::Ping::default())) }).await.unwrap();
    let reply = c6_testkit::within("the old worker's pong, with no note before it", inbound.message()).await.unwrap().unwrap();
    assert!(matches!(reply.msg, Some(c6_proto::head_msg::Msg::Pong(_))), "{reply:?}");
    t.assert_quiet(&session.id, 1, 1).await;
    drop(inbound);
    drop(tx);
    t.close().await;
}
