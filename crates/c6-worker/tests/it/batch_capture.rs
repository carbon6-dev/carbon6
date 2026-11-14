//! Starting several task cards or to-dos together (docs/batch-starts.md),
//! and noting to-dos and follow-ups while a session works, linked to what
//! prompted them (docs/capture.md).

use c6_api::work::{Followup, SessionTodoAdded, TodoStart, WorkItem};
use c6_api::{Connection, Decision, Event, EventKind, Session};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// A head, a worker attached to it and a mock model connection (also for
/// goals.rs).
pub(crate) struct Env {
    pub(crate) mock: MockLlm,
    pub(crate) api: String,
    pub(crate) http: reqwest::Client,
    pub(crate) conn: String,
    pub(crate) wid: uuid::Uuid,
    _head: c6_head::Head,
    _dirs: Vec<tempfile::TempDir>,
    pub(crate) stop: CancellationToken,
}

impl Env {
    pub(crate) async fn new() -> Self {
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
        Env { mock, api, http, conn: conn.id, wid: worker.identity.meta.worker_id.0, _head: head, _dirs: vec![head_dir, state], stop }
    }

    pub(crate) async fn get(&self, path: &str) -> Value {
        self.http.get(format!("{}/{path}", self.api)).send().await.unwrap().json().await.unwrap()
    }

    /// POSTs, failing the test on an error status.
    pub(crate) async fn post(&self, path: &str, body: Value) -> Value {
        let r = self.http.post(format!("{}/{path}", self.api)).json(&body).send().await.unwrap();
        let status = r.status();
        let text = r.text().await.unwrap();
        assert!(status.is_success(), "POST {path}: {status} {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    /// POSTs what should be refused: its error.
    pub(crate) async fn refused(&self, path: &str, body: Value) -> String {
        let r = self.http.post(format!("{}/{path}", self.api)).json(&body).send().await.unwrap();
        assert!(r.status().is_client_error(), "POST {path} should be refused: {}", r.status());
        r.text().await.unwrap()
    }

    pub(crate) async fn start(&self, cwd: &std::path::Path, prompt: &str) -> Session {
        serde_json::from_value(
            self.post("sessions", json!({"worker_id": self.wid, "cwd": cwd, "connection_id": self.conn, "model": "m", "prompt": prompt}))
                .await,
        )
        .unwrap()
    }

    pub(crate) async fn events(&self, id: &str) -> Vec<Event> {
        serde_json::from_value(self.get(&format!("sessions/{id}/events")).await).unwrap()
    }

    /// Its events once `turns` turns have ended and it is idle with
    /// nothing queued.
    pub(crate) async fn ended(&self, id: &str, turns: usize) -> Vec<Event> {
        eventually("the turns to end", async || {
            let ev = self.events(id).await;
            let detail = self.get(&format!("sessions/{id}")).await;
            (ev.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count() == turns
                && detail["session"]["status"] == "idle"
                && detail["queued"].as_array()?.is_empty())
            .then_some(ev)
        })
        .await
    }

    pub(crate) async fn decision(&self, id: i64) -> Decision {
        serde_json::from_value(self.get(&format!("decisions/{id}")).await).unwrap()
    }
}

pub(crate) fn user_texts(events: &[Event]) -> Vec<String> {
    events
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::User { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn card(title: &str) -> (String, String) {
    ("task_propose".into(), json!({"title": title, "why": "Noticed in passing.", "prompt": format!("PROMPT {title}")}).to_string())
}

#[tokio::test]
async fn task_cards_start_together_in_one_session_or_here() {
    let env = Env::new().await;
    let (here, there) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    // One session proposes six cards; another, in another folder, one.
    let titles = ["Card A", "Card B", "Card C", "Card D", "Card E", "Card F"];
    env.mock.push_about("tidy the module", Reply::ToolCalls(titles.iter().map(|t| card(t)).collect()));
    env.mock.push_about("tidy the module", Reply::Text("Proposed six cleanups.".into()));
    let s1 = env.start(here.path(), "tidy the module").await;
    env.ended(&s1.id, 1).await;
    env.mock.push_about("look elsewhere", Reply::ToolCalls(vec![card("Card X")]));
    env.mock.push_about("look elsewhere", Reply::Text("Proposed one.".into()));
    let s2 = env.start(there.path(), "look elsewhere").await;
    env.ended(&s2.id, 1).await;
    let mine: Vec<Decision> = serde_json::from_value(env.get(&format!("sessions/{}/decisions", s1.id)).await).unwrap();
    let id = |t: &str| mine.iter().find(|d| d.title == t).unwrap_or_else(|| panic!("{t}: {mine:?}")).id;
    let theirs: Vec<Decision> = serde_json::from_value(env.get(&format!("sessions/{}/decisions", s2.id)).await).unwrap();
    let x = theirs[0].id;

    // Cards in different folders don't go together; nothing is answered.
    let e = env.refused("decisions/task-batch", json!({"ids": [id("Card F"), x], "how": "start"})).await;
    assert!(e.contains("different places"), "{e}");
    assert_eq!(env.decision(x).await.state, "open");
    assert!(env.refused("decisions/task-batch", json!({"ids": [id("Card A")], "how": "start"})).await.contains("2 to 10"));

    // Three in one session: their prompts in order, each card answered
    // once, and the proposing session hears once.
    env.mock.push_about("Do these tasks, in this order", Reply::Text("Did A, B and C.".into()));
    env.mock.push_about("Start session: task cards", Reply::Text("Noted.".into()));
    let started = env.post("decisions/task-batch", json!({"ids": [id("Card A"), id("Card B"), id("Card C")], "how": "start"})).await;
    let batch = started["session"].as_str().unwrap().to_owned();
    let events = env.ended(&batch, 1).await;
    let EventKind::User { text, .. } = &events.iter().find(|e| matches!(e.kind, EventKind::User { .. })).unwrap().kind else { panic!() };
    let (a, b, c) = (text.find("PROMPT Card A").unwrap(), text.find("PROMPT Card B").unwrap(), text.find("PROMPT Card C").unwrap());
    assert!(a < b && b < c, "in order: {text}");
    for t in ["Card A", "Card B", "Card C"] {
        let d = env.decision(id(t)).await;
        assert_eq!(d.state, "answered", "{d:?}");
        assert_eq!(d.answer.as_ref().unwrap()["option"], "Start it");
        assert_eq!(d.sessions, vec![batch.clone()], "{t}");
    }
    let detail = env.get(&format!("sessions/{batch}")).await;
    assert_eq!(detail["session"]["title"], "Card A + 2 more");
    let heard = user_texts(&env.ended(&s1.id, 2).await);
    let told: Vec<&String> = heard.iter().filter(|t| t.starts_with("Start session: task cards")).collect();
    assert_eq!(told.len(), 1, "{heard:?}");
    assert!(told[0].contains(&format!("session:{batch}")) && told[0].contains("Card C"), "{}", told[0]);

    // Do them here: one message with both, both answered.
    env.mock.push_about("Do these tasks you proposed here", Reply::Text("Doing D and E.".into()));
    let r = env.post("decisions/task-batch", json!({"ids": [id("Card D"), id("Card E")], "how": "here"})).await;
    assert!(r["session"].is_null(), "{r}");
    let heard = user_texts(&env.ended(&s1.id, 3).await);
    let here_msgs: Vec<&String> = heard.iter().filter(|t| t.contains("Do these tasks you proposed here")).collect();
    assert_eq!(here_msgs.len(), 1, "{heard:?}");
    assert!(here_msgs[0].contains("PROMPT Card D") && here_msgs[0].contains("PROMPT Card E"));
    for t in ["Card D", "Card E"] {
        assert_eq!(env.decision(id(t)).await.answer.unwrap()["option"], "Do it here");
    }
    env.stop.cancel();
}

#[tokio::test]
async fn notes_while_it_works_and_to_dos_with_cards_start_together() {
    let env = Env::new().await;
    let here = tempfile::tempdir().unwrap();
    env.mock.push_about("fix the parser", Reply::ToolCalls(vec![card("Card P")]));
    env.mock.push_about("fix the parser", Reply::Text("Retries are disabled here because the client retries.".into()));
    let s = env.start(here.path(), "fix the parser").await;
    let events = env.ended(&s.id, 1).await;
    let reply_seq = events.iter().rev().find(|e| matches!(e.kind, EventKind::Assistant { .. })).unwrap().seq;
    // (The work's note may call the model; the session's agent must not hear.)
    let before = env.events(&s.id).await.len();

    // /todo: on new work named after the session, with its quote; the agent
    // hears nothing.
    let quote = json!({"session": s.id, "seq": reply_seq, "text": "Retries are disabled here"});
    let added: SessionTodoAdded =
        serde_json::from_value(env.post(&format!("sessions/{}/todos", s.id), json!({"text": "Add a retry test", "quote": quote})).await)
            .unwrap();
    let again: SessionTodoAdded =
        serde_json::from_value(env.post(&format!("sessions/{}/todos", s.id), json!({"text": "Check the timeout"})).await).unwrap();
    assert_eq!(added.work, again.work, "the same work");
    let work: WorkItem = serde_json::from_value(env.get(&format!("work/{}", added.work)).await).unwrap();
    assert!(work.fields.attached.iter().any(|a| a.reference == format!("session:{}", s.id)), "it holds the session");
    let first = work.fields.todos.iter().find(|t| t.text == "Add a retry test").unwrap();
    let q = first.quote.as_ref().unwrap();
    assert_eq!((q.session.as_str(), q.seq, q.text.as_str()), (s.id.as_str(), reply_seq, "Retries are disabled here"));
    assert_eq!(env.events(&s.id).await.len(), before, "nothing reached the agent");

    // A follow-up waits in Up next, counted in the list, until sent.
    let f: Followup = serde_json::from_value(
        env.post(&format!("sessions/{}/followups", s.id), json!({"text": "Why not retry once here?", "quote": quote})).await,
    )
    .unwrap();
    let listed: Vec<Session> = serde_json::from_value(env.get("sessions").await).unwrap();
    assert_eq!(listed.iter().find(|x| x.id == s.id).unwrap().followups, 1);
    assert_eq!(env.events(&s.id).await.len(), before, "held, not sent");
    env.mock.push_about("Why not retry once here?", Reply::Text("Because the client already retries.".into()));
    env.post(&format!("sessions/{}/followups/send", s.id), json!({"ids": [f.id]})).await;
    let heard = user_texts(&env.ended(&s.id, 2).await);
    let sent: Vec<&String> = heard.iter().filter(|t| t.contains("Why not retry once here?")).collect();
    assert_eq!(sent.len(), 1, "{heard:?}");
    assert!(sent[0].starts_with("> Retries are disabled here"), "its quote first: {}", sent[0]);
    let left: Vec<Followup> = serde_json::from_value(env.get(&format!("sessions/{}/followups", s.id)).await).unwrap();
    assert!(left.is_empty());

    // The work's picker offers the session's open card; picking it keeps it
    // as a to-do, and one session starts on it with the others.
    let cards: Vec<Decision> = serde_json::from_value(env.get(&format!("work/{}/cards", added.work)).await).unwrap();
    assert_eq!(cards.iter().map(|d| d.title.as_str()).collect::<Vec<_>>(), ["Card P"]);
    let kept = env.post(&format!("work/{}/todos/from-cards", added.work), json!({"ids": [cards[0].id]})).await;
    let card_todo = kept["todos"][0].as_str().unwrap().to_owned();
    assert_eq!(env.decision(cards[0].id).await.state, "answered");
    let work: WorkItem = serde_json::from_value(env.get(&format!("work/{}", added.work)).await).unwrap();
    let ids: Vec<String> = ["Add a retry test", "Check the timeout"]
        .iter()
        .map(|t| work.fields.todos.iter().find(|x| x.text == *t).unwrap().id.clone())
        .chain([card_todo.clone()])
        .collect();
    let plan: TodoStart =
        serde_json::from_value(env.get(&format!("work/{}/todos/start?todos={}", added.work, ids.join(","))).await).unwrap();
    let (a, b, c) = (
        plan.text.find("Add a retry test").unwrap(),
        plan.text.find("Check the timeout").unwrap(),
        plan.text.find("PROMPT Card P").unwrap(),
    );
    assert!(a < b && b < c, "in order: {}", plan.text);
    assert!(plan.text.contains("> Retries are disabled here"), "a to-do's quote goes with it: {}", plan.text);
    assert_eq!(plan.seed.as_deref(), Some(s.id.as_str()));
    env.mock.push_about("Add a retry test", Reply::Text("Did all three.".into()));
    let batch: Session = serde_json::from_value(
        env.post(
            "sessions",
            json!({"worker_id": env.wid, "cwd": here.path(), "connection_id": env.conn, "model": "m", "prompt": plan.text,
                   "work": added.work, "todo": ids[0], "todos": ids[1..]}),
        )
        .await,
    )
    .unwrap();
    assert_eq!(batch.title, "Add a retry test + 2 more");
    let work: WorkItem = serde_json::from_value(env.get(&format!("work/{}", added.work)).await).unwrap();
    for id in &ids {
        assert_eq!(work.fields.todos.iter().find(|t| &t.id == id).unwrap().session, batch.id, "to-do {id} links to it");
    }

    // Archived with a follow-up unsent: it becomes a to-do, not lost.
    env.post(&format!("sessions/{}/followups", s.id), json!({"text": "Ask about the fixture"})).await;
    let r = env.http.patch(format!("{}/sessions/{}", env.api, s.id)).json(&json!({"archived": true})).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    let work: WorkItem = serde_json::from_value(env.get(&format!("work/{}", added.work)).await).unwrap();
    assert!(work.fields.todos.iter().any(|t| t.text == "Ask about the fixture"), "{:?}", work.fields.todos);
    let left: Vec<Followup> = serde_json::from_value(env.get(&format!("sessions/{}/followups", s.id)).await).unwrap();
    assert!(left.is_empty());
    env.stop.cancel();
}
