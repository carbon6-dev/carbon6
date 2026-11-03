//! Sessions name themselves (docs/suggestions.md §7): once a session's
//! first turn finishes, the model a `task:name` routing rule picks gives it
//! a short name in place of its first line, once; a first message of a few
//! words waits for the next turn. A name you give it (or an Undo) wins,
//! none is made where it's off, and subagents and side chats keep their
//! titles. Rename's Suggest makes one on demand, without applying it.

use std::path::Path;
use std::sync::Arc;

use c6_api::{Event, EventKind, Session, SessionDetail, SessionStatus, StreamMsg};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, NAMING, Reply, eventually};
use c6_worker::{ConnState, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

struct Env {
    head: c6_head::Head,
    api: String,
    http: reqwest::Client,
    mock: MockLlm,
    conn_id: String,
    _head_dir: tempfile::TempDir,
}

impl Env {
    async fn new() -> Self {
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
        let mock = MockLlm::start().await;
        let conn: Value = http
            .post(format!("{api}/connections"))
            .json(&json!({"name": "m", "base_url": mock.base_url()}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let env = Self { head, api, http, mock, conn_id: conn["id"].as_str().unwrap().to_owned(), _head_dir: head_dir };
        // Names go to a small model.
        env.put(
            "/settings/routes",
            json!([{"name": "names", "applies_to": ["task:name"], "connection": env.conn_id, "model": "small", "enabled": true}]),
        )
        .await;
        env
    }

    async fn worker(&self, state: &Path) -> (Arc<Worker>, CancellationToken) {
        let e = c6_head::api::new_enrollment(&self.head.state, None).await.unwrap();
        enroll::enroll(&e.token).await.unwrap().save(state).unwrap();
        let w = crate::open_worker(state).await;
        let (tx, mut rx) = watch::channel(ConnState::Connecting);
        let stop = CancellationToken::new();
        tokio::spawn(conn::run(w.clone(), tx, stop.clone()));
        c6_testkit::within("the worker to attach", rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap();
        (w, stop)
    }

    async fn put(&self, path: &str, body: Value) {
        let r = self.http.put(format!("{}{path}", self.api)).json(&body).send().await.unwrap();
        assert!(r.status().is_success(), "{path}: {}", r.text().await.unwrap());
    }

    async fn post<T: serde::de::DeserializeOwned>(&self, path: &str, body: Value) -> T {
        let r = self.http.post(format!("{}{path}", self.api)).json(&body).send().await.unwrap();
        assert!(r.status().is_success(), "{path}: {}", r.text().await.unwrap());
        r.json().await.unwrap()
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        let r = self.http.get(format!("{}{path}", self.api)).send().await.unwrap();
        assert!(r.status().is_success(), "{path}: {}", r.status());
        r.json().await.unwrap()
    }

    async fn create(&self, worker: &Worker, prompt: &str) -> Session {
        self.post(
            "/sessions",
            json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": "", "connection_id": self.conn_id, "model": "m", "prompt": prompt}),
        )
        .await
    }

    async fn say(&self, id: &str, text: &str) {
        let r = self.http.post(format!("{}/sessions/{id}/messages", self.api)).json(&json!({"text": text})).send().await.unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    }

    async fn rename(&self, id: &str, title: &str) {
        let r = self.http.patch(format!("{}/sessions/{id}", self.api)).json(&json!({"title": title})).send().await.unwrap();
        assert!(r.status().is_success(), "{}", r.text().await.unwrap());
    }

    async fn title(&self, id: &str) -> String {
        self.get::<SessionDetail>(&format!("/sessions/{id}")).await.session.title
    }

    /// Waits until the session is idle with `turns` finished turns.
    async fn idle(&self, id: &str, turns: usize) -> Vec<Event> {
        eventually("the turn to finish", async || {
            let d: SessionDetail = self.get(&format!("/sessions/{id}")).await;
            if d.session.status != SessionStatus::Idle || !d.queued.is_empty() {
                return None;
            }
            let evs: Vec<Event> = self.get(&format!("/sessions/{id}/events")).await;
            (evs.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count() == turns).then_some(evs)
        })
        .await
    }

    /// Where the session is in waiting for its name (None: not waiting).
    async fn waiting(&self, id: &str) -> Option<String> {
        self.head.state.store.setting(&format!("session_naming:{id}")).await.unwrap()
    }

    /// The naming calls the model took, as their conversations' text.
    fn naming_calls(&self) -> Vec<(String, String)> {
        self.mock
            .requests()
            .iter()
            .filter(|r| r["messages"].to_string().contains(NAMING))
            .map(|r| (r["model"].as_str().unwrap_or("").to_owned(), r["messages"].to_string()))
            .collect()
    }
}

#[tokio::test]
async fn sessions_name_themselves_after_their_first_turn() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    let (worker, stop) = env.worker(state.path()).await;

    // 1. The first turn finishes: the small model names it, in place of its
    // first line, and whoever looks hears what it was (for Undo).
    let gate = env.mock.push_gated(Reply::Text("Fixed the off-by-one in the pager.".into()));
    env.mock.push_about(NAMING, Reply::Text("\"pager off-by-one fix\".".into()));
    let s = env.create(&worker, "fix the pager bug in the list view").await;
    assert_eq!(s.title, "fix the pager bug in the list view");
    let mut live = env.head.state.hub.subscribe(&s.id);
    gate.notify_one();
    env.idle(&s.id, 1).await;
    eventually("the session named", async || (env.title(&s.id).await == "Pager off-by-one fix").then_some(())).await;
    let named = eventually("word of its name", async || {
        loop {
            match live.try_recv() {
                Ok(StreamMsg::Named { title, from }) => return Some((title, from)),
                Ok(_) => {}
                Err(_) => return None,
            }
        }
    })
    .await;
    assert_eq!(named, ("Pager off-by-one fix".to_owned(), "fix the pager bug in the list view".to_owned()));
    let calls = env.naming_calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0].0, "small", "the task:name rule's model");
    assert!(calls[0].1.contains("fix the pager bug") && calls[0].1.contains("Fixed the off-by-one"), "{}", calls[0].1);
    assert_eq!(env.waiting(&s.id).await, None);

    // 2. Named once: a later turn leaves it.
    env.mock.push_about(NAMING, Reply::Text("Footer work".into()));
    env.mock.push(Reply::Text("Done.".into()));
    env.say(&s.id, "now the footer").await;
    env.idle(&s.id, 2).await;
    assert_eq!(env.title(&s.id).await, "Pager off-by-one fix");

    // 3. Undo puts its first line back, as your choice: never named again.
    env.rename(&s.id, "fix the pager bug in the list view").await;
    env.mock.push(Reply::Text("Done again.".into()));
    env.say(&s.id, "and the header").await;
    env.idle(&s.id, 3).await;
    assert_eq!(env.title(&s.id).await, "fix the pager bug in the list view");
    assert_eq!(env.naming_calls().len(), 1, "no second name");

    // 4. Rename's Suggest: a name to start from, not applied.
    let suggested: Value = env.get(&format!("/sessions/{}/name/suggestion", s.id)).await;
    assert_eq!(suggested["title"], "Footer work", "{suggested}");
    assert_eq!(env.title(&s.id).await, "fix the pager bug in the list view");
    assert!(env.naming_calls()[1].1.contains("fix the pager bug"), "the same call: how it started");

    stop.cancel();
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_first_message_of_a_few_words_waits_for_the_next_turn() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    let (worker, stop) = env.worker(state.path()).await;

    env.mock.push(Reply::Text("Hello! What should we work on?".into()));
    let s = env.create(&worker, "hi").await;
    env.idle(&s.id, 1).await;
    eventually("waiting for the next turn", async || (env.waiting(&s.id).await.as_deref() == Some("next")).then_some(())).await;
    assert_eq!(env.title(&s.id).await, "hi");
    assert!(env.naming_calls().is_empty());

    env.mock.push(Reply::Text("Switching realms reloads every pane; I'll cache them.".into()));
    env.mock.push_about(NAMING, Reply::Text("Realm switching speed".into()));
    env.say(&s.id, "switching realms is slow, can you look").await;
    env.idle(&s.id, 2).await;
    eventually("the session named", async || (env.title(&s.id).await == "Realm switching speed").then_some(())).await;
    let calls = env.naming_calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].1.contains("User: hi") && calls[0].1.contains("switching realms is slow"), "both turns: {}", calls[0].1);

    stop.cancel();
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn your_names_and_the_setting_win() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    let (worker, stop) = env.worker(state.path()).await;
    // Any naming call would be taken, and seen.
    env.mock.push_about(NAMING, Reply::Text("Not this".into()));

    // 1. Renamed by you before its first turn ends.
    let gate = env.mock.push_gated(Reply::Text("Looked into it.".into()));
    let s = env.create(&worker, "look into the flaky upload test").await;
    env.rename(&s.id, "Upload flake").await;
    assert_eq!(env.waiting(&s.id).await, None, "yours: never named by itself");
    gate.notify_one();
    env.idle(&s.id, 1).await;
    assert_eq!(env.title(&s.id).await, "Upload flake");

    // 2. Named when started: it keeps that name.
    env.mock.push(Reply::Text("Checked.".into()));
    let given: Session = env
        .post(
            "/sessions",
            json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": "", "connection_id": env.conn_id, "model": "m",
                "prompt": "check the backups ran last night", "title": "Backup check"}),
        )
        .await;
    assert_eq!(env.waiting(&given.id).await, None);
    env.idle(&given.id, 1).await;
    assert_eq!(env.title(&given.id).await, "Backup check");

    // 3. Off for the realm: none made (and none later).
    env.put("/settings/suggestions", json!({"enabled": true, "names": false})).await;
    let off: Value = env.get("/settings/suggestions").await;
    assert_eq!(off["names"], false, "{off}");
    env.mock.push(Reply::Text("Rotated them.".into()));
    let s = env.create(&worker, "rotate the staging certificates").await;
    env.idle(&s.id, 1).await;
    eventually("given up on its name", async || env.waiting(&s.id).await.is_none().then_some(())).await;
    assert_eq!(env.title(&s.id).await, "rotate the staging certificates");
    assert!(env.naming_calls().is_empty(), "{:?}", env.naming_calls());

    stop.cancel();
    env.head.shutdown().await.unwrap();
}

#[tokio::test]
async fn subagents_and_side_chats_keep_their_titles_and_the_assistant_s_are_named() {
    let env = Env::new().await;
    let state = tempfile::tempdir().unwrap();
    let (worker, stop) = env.worker(state.path()).await;
    let project = tempfile::tempdir().unwrap();
    env.mock.push_about(NAMING, Reply::Text("Needle search".into()));

    // A session whose agent starts a subagent.
    env.mock.push(Reply::ToolCalls(vec![("task".into(), r#"{"description":"dig","prompt":"Look for the needle."}"#.into())]));
    env.mock.push(Reply::Text("Found it in hay.txt.".into())); // the subagent
    env.mock.push(Reply::Text("The needle is in hay.txt.".into())); // the parent
    let s: Session = env
        .post(
            "/sessions",
            json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path().display().to_string(),
                "connection_id": env.conn_id, "model": "m", "prompt": "find the needle in this folder"}),
        )
        .await;
    let evs = env.idle(&s.id, 1).await;
    eventually("the session named", async || (env.title(&s.id).await == "Needle search").then_some(())).await;
    let children: Vec<Session> = env.get(&format!("/sessions/{}/children", s.id)).await;
    let child = children.first().expect("a subagent");
    // (It finished before its parent's turn did.)
    assert_eq!(env.waiting(&child.id).await, None);
    assert_eq!(env.title(&child.id).await, child.title);

    // A side chat on it.
    let anchor = evs.iter().find(|e| matches!(e.kind, EventKind::Assistant { .. })).unwrap().seq;
    env.mock.push(Reply::Text("Because it was told to.".into()));
    let side: Session = env
        .post(
            &format!("/sessions/{}/side-chats", s.id),
            json!({"anchor_seq": anchor, "connection_id": env.conn_id, "model": "m", "prompt": "why did it start a subagent for this?"}),
        )
        .await;
    assert_eq!(env.waiting(&side.id).await, None);
    env.idle(&side.id, 1).await;
    assert_eq!(env.title(&side.id).await, "why did it start a subagent for this?");
    assert_eq!(env.naming_calls().len(), 1, "only the session's own");

    // A conversation with the assistant is named.
    env.put("/settings/assistant", json!({"model": {"connection": "m", "model": "mock-1"}})).await;
    env.mock.push(Reply::Text("Realms are made under Settings → Realms.".into()));
    env.mock.push_about(NAMING, Reply::Text("Adding a realm".into()));
    let a: Session = env.post("/assistant", json!({"prompt": "how do I add a realm?"})).await;
    eventually("the conversation named", async || (env.title(&a.id).await == "Adding a realm").then_some(())).await;

    stop.cancel();
    env.head.shutdown().await.unwrap();
}
