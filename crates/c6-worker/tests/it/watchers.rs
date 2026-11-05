//! Watchers (docs/watchers.md): standing agents that follow sessions. A
//! wake reads what changed and calls its model only when something it wakes
//! on happened; each such wake is a turn of its own session, which you can
//! read and write to. Stuck detection nudges a looping agent; the
//! retrospective turns a correction into memory notes, a standard (the
//! review watcher then checks changes against it) or a question for you; the
//! guard holds a risky call for you; the improvement watcher turns repeated
//! failures into task cards; the consistency watcher keeps a ledger.

use c6_api::watchers::{Finding, SessionWatch, Watcher, WatcherDef};
use c6_api::{Event, EventKind, Session};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use reqwest::Method as M;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// In every session's requests, never in a watcher's.
const SESSION: &str = "You are carbon6";
/// The agendas' openings (their defaults).
const STUCK: &str = "for being stuck";
const RETRO: &str = "continuous retrospective";
const REVIEW: &str = "You review an AI agent's changes";
const GUARD: &str = "is about to run";
const IMPROVE: &str = "You find improvements to carbon6";
const WEEKLY: &str = "Write this week's retrospective";
const LEDGER: &str = "You keep sessions consistent";

struct Env {
    head: c6_head::Head,
    api: String,
    http: reqwest::Client,
    mock: MockLlm,
    worker: std::sync::Arc<Worker>,
    conn: Value,
    stop: CancellationToken,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

impl Env {
    async fn new() -> Self {
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
        let conn: Value = http
            .post(format!("{api}/connections"))
            .json(&json!({"name": "m", "base_url": mock.base_url()}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let env = Self { head, api, http, mock, worker, conn, stop, _dirs: (head_dir, state) };
        // Only what each test is about asks a model.
        env.send(M::PUT, "settings/work", json!({"guess": false, "notes": false, "quiet_days": 3, "nudge": false, "needs_hours": 4})).await;
        env.send(M::PUT, "settings/suggestions", json!({"enabled": false, "catchup": false, "fold": false, "names": false})).await;
        for w in ["stuck", "retro", "review", "retro-weekly"] {
            env.set(w, |d| d.enabled = false).await;
        }
        env
    }

    async fn send(&self, method: M, path: &str, body: Value) -> Value {
        let r = self.http.request(method, format!("{}/{path}", self.api)).json(&body).send().await.unwrap();
        let ok = r.status().is_success();
        let text = r.text().await.unwrap();
        assert!(ok, "{path}: {text}");
        serde_json::from_str(&text).unwrap_or(Value::Null)
    }

    async fn get<T: serde::de::DeserializeOwned>(&self, path: &str) -> T {
        self.http.get(format!("{}/{path}", self.api)).send().await.unwrap().json().await.unwrap()
    }

    /// Changes a watcher's definition.
    async fn set(&self, name: &str, f: impl Fn(&mut WatcherDef)) {
        let all: Vec<Watcher> = self.get("watchers").await;
        let mut d = all.into_iter().find(|w| w.def.name == name).unwrap().def;
        f(&mut d);
        self.send(M::PUT, &format!("watchers/{name}"), serde_json::to_value(d).unwrap()).await;
    }

    async fn start(&self, prompt: &str) -> Session {
        serde_json::from_value(
            self.send(
                M::POST,
                "sessions",
                json!({"worker_id": self.worker.identity.meta.worker_id.0, "cwd": "", "connection_id": self.conn["id"], "model": "m",
                       "prompt": prompt, "work": "none"}),
            )
            .await,
        )
        .unwrap()
    }

    async fn events(&self, id: &str) -> Vec<Event> {
        self.get(&format!("sessions/{id}/events")).await
    }

    /// Waits until `id` has ended `n` turns and is idle.
    async fn turns(&self, id: &str, n: usize) -> Vec<Event> {
        eventually("the turns to end", async || {
            let d: Value = self.http.get(format!("{}/sessions/{id}", self.api)).send().await.ok()?.json().await.ok()?;
            let ev = self.events(id).await;
            let ended = ev.iter().filter(|e| matches!(e.kind, EventKind::TurnEnded { .. })).count();
            (d["session"]["status"] == "idle" && ended >= n).then_some(ev)
        })
        .await
    }

    async fn findings(&self, watcher: &str) -> Vec<Finding> {
        self.get(&format!("watchers/findings?watcher={watcher}")).await
    }

    /// Waits for a finding of `watcher` that `f` accepts.
    async fn found(&self, watcher: &str, f: impl Fn(&Finding) -> bool) -> Finding {
        c6_testkit::eventually_or(
            "the watcher's finding",
            async || self.findings(watcher).await.into_iter().find(|x| f(x)),
            || self.mock.log(),
        )
        .await
    }

    /// Waits until no watcher is waking.
    async fn settled(&self) {
        eventually("the watchers to settle", async || (!c6_head::watchers::busy(&self.head.state)).then_some(())).await;
    }
}

fn answer(findings: Value) -> Reply {
    Reply::Text(json!({"findings": findings, "memory": "noted"}).to_string())
}

fn text_of(e: &Event) -> String {
    match &e.kind {
        EventKind::User { text, .. } => text.clone(),
        EventKind::Assistant { blocks, .. } => format!("{blocks:?}"),
        _ => String::new(),
    }
}

#[tokio::test]
async fn stuck_detection_nudges_a_looping_agent_and_answers_you() {
    let env = Env::new().await;
    env.set("stuck", |d| d.enabled = true).await;
    let loop_call = Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo hi"}"#.into())]);
    for _ in 0..3 {
        env.mock.push_about_all(&[SESSION, "check the build"], loop_call.clone());
    }
    env.mock.push_about_all(&[SESSION, "check the build"], Reply::Text("Done checking.".into()));
    env.mock.push_about(
        STUCK,
        answer(json!([{"severity": "important", "kind": "note", "text": "It runs echo hi again and again.",
                        "note": "You ran `echo hi` three times with the same result; try something else."}])),
    );
    env.mock.push_about_all(&[SESSION, "try something else"], Reply::Text("Understood: stopping the loop.".into()));
    let s = env.start("check the build").await;

    // The nudge reaches the agent, as a message from the watcher.
    let sent = env.found("stuck", |f| f.done == "sent" && f.watched == s.id).await;
    assert_eq!(sent.kind, "note");
    let ev = env.turns(&s.id, 2).await;
    assert!(
        ev.iter().any(|e| matches!(&e.kind, EventKind::User { text, from_title, .. } if text.contains("try something else") && from_title.contains("Stuck detection"))),
        "the agent got the nudge"
    );

    // The watcher's own session: the wake as a turn, with what woke it.
    let watch: SessionWatch = env.get(&format!("sessions/{}/watch", s.id)).await;
    let (_, wsid) = watch.watchers.iter().find(|(n, _)| n == "stuck").cloned().unwrap();
    let wev = env.turns(&wsid, 1).await;
    assert!(wev.iter().any(|e| text_of(e).starts_with("Woke on: loop")), "{wev:?}");
    assert!(wev.iter().any(|e| text_of(e).contains("echo hi again")), "its answer");
    assert!(wev.iter().any(|e| matches!(e.kind, EventKind::Usage { .. })), "its tokens count under it");
    // Later turns of the watched session held nothing it wakes on: skips,
    // with no model call.
    env.settled().await;
    let list: Vec<Watcher> = env.get("watchers").await;
    let w = list.iter().find(|w| w.def.name == "stuck").unwrap();
    assert_eq!(w.woke, 1, "one wake with a model call");
    assert!(w.skipped >= 1, "the rest were skips");
    assert_eq!(env.mock.requests().iter().filter(|r| r["messages"].to_string().contains(STUCK)).count(), 1);

    // You can write to it; its answer is a turn of its session.
    env.mock.push_about_all(
        &[STUCK, "why did you nudge it"],
        Reply::Text(json!({"findings": [], "reply": "It repeated the same call three times."}).to_string()),
    );
    env.send(M::POST, &format!("sessions/{wsid}/messages"), json!({"text": "why did you nudge it?"})).await;
    let wev = env.turns(&wsid, 2).await;
    assert!(wev.iter().any(|e| text_of(e).contains("repeated the same call")), "its reply");
    env.stop.cancel();
}

#[tokio::test]
async fn a_correction_becomes_lessons_and_a_standard_that_review_checks() {
    let env = Env::new().await;
    env.set("retro", |d| d.enabled = true).await;
    env.mock.push_about_all(&[SESSION, "set up the cache"], Reply::Text("Set it up with npm.".into()));
    let s = env.start("set up the cache").await;
    env.turns(&s.id, 1).await;
    let worker_label = format!("worker:{}", env.worker.identity.meta.worker_name);

    // You correct it: the retrospective wakes.
    env.mock.push_about_all(&[SESSION, "don't use npm"], Reply::Text("Switching to pnpm.".into()));
    env.mock.push_about(
        RETRO,
        answer(json!([
            {"severity": "important", "kind": "lesson", "text": "This machine uses pnpm.", "scope": worker_label, "note": "Use pnpm, not npm, on this machine."},
            {"severity": "info", "kind": "lesson", "text": "Ask before adding dependencies.", "scope": "", "note": "Ask before adding a dependency."},
            {"severity": "important", "kind": "directive", "text": "Memory accounting asked for once.", "directive": "Account for memory in every function that allocates.",
             "scope": "", "checkable": false, "audit": "Check every function that allocates accounts for its memory."}
        ])),
    );
    env.send(
        M::POST,
        &format!("sessions/{}/messages", s.id),
        json!({"text": "no, don't use npm here; make sure memory is accounted for in this function"}),
    )
    .await;

    // Narrow: applied. Wider: asked. The directive: how far should it go?
    let applied = env.found("retro", |f| f.kind == "lesson" && f.done == "applied").await;
    assert!(applied.reference.starts_with("memory:"), "{applied:?}");
    let notes: Vec<c6_api::Memory> = env.get("memories").await;
    assert!(notes.iter().any(|n| n.text == "Use pnpm, not npm, on this machine." && n.scope == worker_label), "{notes:?}");
    // Recorded as asked first; the reference says the decision exists.
    env.found("retro", |f| f.kind == "lesson" && f.done == "asked" && f.reference.starts_with("decision:")).await;
    env.found("retro", |f| f.kind == "directive" && f.done == "asked" && f.reference.starts_with("decision:")).await;
    let decisions: Vec<Value> = env.get("decisions").await;
    let ask = |t: &str| {
        decisions.iter().find(|d| d["title"].as_str().unwrap_or("").contains(t)).cloned().unwrap_or_else(|| panic!("{t}: {decisions:?}"))
    };
    let lesson = ask("Remember this for every session");
    let directive = ask("How far should this go");
    assert_eq!(directive["options"].as_array().unwrap().len(), 3);
    env.send(M::POST, &format!("decisions/{}/answer", lesson["id"]), json!({"option": "Allow"})).await;
    env.send(M::POST, &format!("decisions/{}/answer", directive["id"]), json!({"option": "From now on"})).await;
    let notes: Vec<c6_api::Memory> = env.get("memories").await;
    assert!(notes.iter().any(|n| n.text == "Ask before adding a dependency."), "allowed: {notes:?}");
    let standard = notes.iter().find(|n| n.text.starts_with("Account for memory")).unwrap_or_else(|| panic!("{notes:?}"));
    assert_eq!(standard.enforced_by, "review", "a standard the review watcher checks");

    // Review, on every turn here: it reads the standard.
    env.turns(&s.id, 2).await;
    env.settled().await;
    env.set("review", |d| {
        d.enabled = true;
        d.applies_to.clear();
        d.wakes = vec!["turn_end".into()];
    })
    .await;
    env.mock.push_about_all(&[SESSION, "add the eviction"], Reply::Text("Added eviction.".into()));
    env.mock.push_about(REVIEW, answer(json!([{"severity": "important", "kind": "note", "text": "Eviction frees nothing it counted.", "note": "Account for the memory eviction frees."}])));
    env.mock.push_about_all(&[SESSION, "Account for the memory eviction frees"], Reply::Text("Fixed.".into()));
    // Its looks at the fix: nothing more.
    for _ in 0..3 {
        env.mock.push_about(REVIEW, answer(json!([])));
    }
    env.send(M::POST, &format!("sessions/{}/messages", s.id), json!({"text": "add the eviction"})).await;
    env.found("review", |f| f.done == "sent").await;
    let asked = env.mock.requests().into_iter().find(|r| r["messages"].to_string().contains(REVIEW)).unwrap();
    assert!(asked["messages"].to_string().contains("Account for memory in every function"), "the standard is in its prompt");

    // The weekly roll-up writes a document from the week's findings.
    env.settled().await;
    env.set("retro-weekly", |d| d.enabled = true).await;
    env.mock.push_about(WEEKLY, answer(json!([{"severity": "info", "kind": "doc", "text": "This week's retrospective", "title": "Retro week 45", "doc": "## Learned\n- pnpm on this machine."}])));
    env.send(M::POST, "watchers/retro-weekly/run", json!({})).await;
    let doc = env.found("retro-weekly", |f| f.kind == "doc").await;
    assert!(doc.reference.starts_with("doc:"), "{doc:?}");
    let asked = env.mock.requests().into_iter().find(|r| r["messages"].to_string().contains(WEEKLY)).unwrap();
    assert!(asked["messages"].to_string().contains("pnpm"), "it read the week's findings");
    env.stop.cancel();
}

#[tokio::test]
async fn the_guard_holds_a_risky_call_for_you() {
    let env = Env::new().await;
    env.set("guard", |d| {
        d.enabled = true;
        d.pattern = "danger-zone".into();
    })
    .await;
    env.mock.push_about_all(&[SESSION, "clean up"], Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo danger-zone"}"#.into())]));
    env.mock.push_about(GUARD, answer(json!([{"severity": "urgent", "kind": "risky", "text": "It deletes the danger zone."}])));
    env.mock.push_about_all(&[SESSION, "clean up"], Reply::Text("Cleaned.".into()));
    let s = env.start("clean up").await;
    let call = eventually("the call to wait for you", async || {
        env.events(&s.id).await.into_iter().find_map(|e| match e.kind {
            EventKind::Approval { call_id, state, message, .. } if state == "requested" => Some((call_id, message)),
            _ => None,
        })
    })
    .await;
    assert_eq!(call.1, "It deletes the danger zone.", "the guard's reason");
    env.found("guard", |f| f.kind == "risky").await;
    env.send(M::POST, &format!("sessions/{}/approvals/{}", s.id, call.0), json!({"allow": true})).await;
    let ev = env.turns(&s.id, 1).await;
    assert!(
        ev.iter().any(|e| matches!(&e.kind, EventKind::ToolResult { output, .. } if output.contains("danger-zone"))),
        "allowed: it ran"
    );
    env.stop.cancel();
}

#[tokio::test]
async fn repeated_failures_become_cards_and_sessions_keep_a_ledger() {
    let env = Env::new().await;
    // carbon6's own failures: counted without a model; three make a group.
    env.set("improve", |d| d.enabled = true).await;
    // Its model, by a routing rule (there's no session yet to follow).
    env.send(M::PUT, "settings/routes", json!([{"name": "watchers", "applies_to": ["task:watch:improve"], "connection": env.conn["id"], "model": "small", "enabled": true}])).await;
    for n in 0..3 {
        env.send(M::POST, "ui/error", json!({"page": "/work", "message": format!("panicked at work.rs: index {n} out of range")})).await;
    }
    env.mock.push_about(
        IMPROVE,
        answer(
            json!([{"severity": "important", "kind": "card", "title": "Fix the work page's index panic", "why": "The web UI broke 3 times.",
                        "prompt": "In the carbon6 repo, the work page panics with an index out of range; find and fix it."}]),
        ),
    );
    env.send(M::POST, "watchers/improve/run", json!({})).await;
    let card = env.found("improve", |f| f.kind == "card").await;
    assert!(card.reference.starts_with("decision:"), "{card:?}");
    let asked = env.mock.requests().into_iter().find(|r| r["messages"].to_string().contains(IMPROVE)).unwrap();
    assert!(asked["messages"].to_string().contains("3×"), "grouped and counted: {asked}");
    assert_eq!(asked["model"], "small", "routed as task:watch:improve");
    // Nothing new since: a skip, no model call.
    env.send(M::POST, "watchers/improve/run", json!({})).await;
    assert_eq!(env.mock.requests().iter().filter(|r| r["messages"].to_string().contains(IMPROVE)).count(), 1);

    // The consistency watcher: a decision made in a session goes in the ledger.
    env.set("consistency", |d| d.enabled = true).await;
    env.mock.push_about_all(&[SESSION, "name the table"], Reply::Text("Called it user_events.".into()));
    env.mock.push_about(
        LEDGER,
        answer(json!([{"severity": "info", "kind": "ledger", "text": "Table name", "entry": "The events table is user_events."}])),
    );
    let s = env.start("name the table").await;
    let entry = env.found("consistency", |f| f.kind == "ledger" && f.watched == s.id).await;
    let doc = entry.reference.strip_prefix("doc:").unwrap().to_owned();
    let read: Value = env.get(&format!("docs/{doc}/read?view=markdown")).await;
    assert!(read.to_string().contains("user_events"), "{read}");
    env.stop.cancel();
}

/// A watcher's session, notes and position live in the head's database:
/// after a restart you can still write to it, and it remembers.
#[tokio::test]
async fn a_watcher_outlasts_a_head_restart() {
    let mock = MockLlm::start().await;
    let dir = tempfile::tempdir().unwrap();
    let start = || {
        c6_head::start(HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            ..Default::default()
        })
    };
    let head = start().await.unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let http = reqwest::Client::new();
    let conn: Value = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let put = |path: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { assert!(http.put(&url).json(&body).send().await.unwrap().status().is_success()) }
    };
    put(
        "settings/routes",
        json!([{"name": "w", "applies_to": ["task:watch:improve"], "connection": conn["id"], "model": "small", "enabled": true}]),
    )
    .await;
    let all: Vec<Watcher> = http.get(format!("{api}/watchers")).send().await.unwrap().json().await.unwrap();
    let mut d = all.into_iter().find(|w| w.def.name == "improve").unwrap().def;
    d.enabled = true;
    put("watchers/improve", serde_json::to_value(&d).unwrap()).await;
    for _ in 0..3 {
        http.post(format!("{api}/ui/error")).json(&json!({"page": "/", "message": "the same crash"})).send().await.unwrap();
    }
    mock.push_about(IMPROVE, Reply::Text(json!({"findings": [], "memory": "Seen: the same crash on /, 3 times."}).to_string()));
    assert!(http.post(format!("{api}/watchers/improve/run")).json(&json!({})).send().await.unwrap().status().is_success());
    let found: Vec<Finding> = http.get(format!("{api}/watchers/findings")).send().await.unwrap().json().await.unwrap();
    assert!(found.is_empty());
    head.shutdown().await.unwrap();

    let head = start().await.unwrap();
    let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
    let sid: String = sqlx_session(&head).await;
    mock.push_about_all(
        &[IMPROVE, "what have you seen"],
        Reply::Text(json!({"findings": [], "reply": "The same crash on /, three times."}).to_string()),
    );
    assert!(
        http.post(format!("{api}/sessions/{sid}/messages"))
            .json(&json!({"text": "what have you seen?"}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    let ev = eventually("its answer after the restart", async || {
        let ev: Vec<Event> = http.get(format!("{api}/sessions/{sid}/events")).send().await.ok()?.json().await.ok()?;
        ev.iter().any(|e| text_of(e).contains("three times")).then_some(ev)
    })
    .await;
    assert!(ev.iter().any(|e| text_of(e).contains("what have you seen")));
    let asked = mock.requests().into_iter().rfind(|r| r["messages"].to_string().contains(IMPROVE)).unwrap();
    assert!(asked["messages"].to_string().contains("Seen: the same crash"), "its notes came back with it");
    head.shutdown().await.unwrap();
}

/// The improvement watcher's session.
async fn sqlx_session(head: &c6_head::Head) -> String {
    let list = c6_head::watchers::findings(&head.state, &Default::default(), None).await.unwrap();
    assert!(list.is_empty());
    sqlx::query_scalar("SELECT session_id FROM watcher_state WHERE watcher = 'improve'").fetch_one(head.state.store.pool()).await.unwrap()
}

/// A head run from its config runs the shipped watchers as they ship (some
/// on); one built in code starts them off.
#[tokio::test]
async fn shipped_watchers_start_as_shipped_from_a_config_file() {
    for (watchers, on) in [(None, false), (Some(true), true)] {
        let dir = tempfile::tempdir().unwrap();
        let head = c6_head::start(HeadConfig {
            data_dir: dir.path().to_owned(),
            bind: vec!["127.0.0.1".parse().unwrap()],
            ui_port: 0,
            worker_port: 0,
            auth: false,
            watchers,
            ..Default::default()
        })
        .await
        .unwrap();
        let api = format!("http://127.0.0.1:{}/api/v1", head.ui_addrs[0].port());
        let all: Vec<Watcher> = reqwest::get(format!("{api}/watchers")).await.unwrap().json().await.unwrap();
        let enabled = |n: &str| all.iter().find(|w| w.def.name == n).unwrap().def.enabled;
        assert_eq!((enabled("stuck"), enabled("retro"), enabled("review"), enabled("retro-weekly")), (on, on, on, on));
        assert!(!enabled("guard") && !enabled("improve") && !enabled("consistency"), "these ship off");
        head.shutdown().await.unwrap();
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c6h.toml");
    std::fs::write(&path, "auth = false\n").unwrap();
    assert_eq!(HeadConfig::load(&path).unwrap().watchers, Some(true));
    std::fs::write(&path, "watchers = false\n").unwrap();
    assert_eq!(HeadConfig::load(&path).unwrap().watchers, Some(false));
}
