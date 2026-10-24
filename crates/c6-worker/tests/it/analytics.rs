//! Analytics (docs/analytics.md): turns, model and tool time, your
//! messages and corrections, by day, model, tool and label key; the same
//! report again from the event log after the marks are gone.

use c6_api::{AnalyticsReport, Connection, Session};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply, eventually};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn analytics_counts_work_and_you() {
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
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ended = |id: String, turns: usize| {
        let (http, api) = (http.clone(), api.clone());
        async move {
            eventually("the turn to end", async || {
                let d: Value = http.get(format!("{api}/sessions/{id}")).send().await.ok()?.json().await.ok()?;
                let ev: Vec<Value> = http.get(format!("{api}/sessions/{id}/events")).send().await.ok()?.json().await.ok()?;
                (d["session"]["status"] == "idle" && ev.iter().filter(|e| e["type"] == "turn_ended").count() >= turns).then_some(())
            })
            .await
        }
    };

    // A task with a tool call, then a correction.
    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo hi"}"#.into())]));
    mock.push(Reply::Text("Said hi.".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path(), "connection_id": conn.id, "model": "m1",
                      "prompt": "say hi", "labels": ["client:acme"]}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    ended(s.id.clone(), 1).await;
    mock.push(Reply::Text("Said hello.".into()));
    assert!(
        http.post(format!("{api}/sessions/{}/messages", s.id))
            .json(&json!({"text": "say hello instead"}))
            .send()
            .await
            .unwrap()
            .status()
            .is_success()
    );
    ended(s.id.clone(), 2).await;

    let report = || {
        let (http, api) = (http.clone(), api.clone());
        async move { http.get(format!("{api}/analytics?days=1&by=client")).send().await.unwrap().json::<AnalyticsReport>().await.unwrap() }
    };
    let r = report().await;
    let t = &r.total;
    assert_eq!((t.sessions, t.tasks, t.turns, t.messages, t.steers), (1, 1, 2, 2, 1), "{t:?}");
    assert_eq!((t.calls, t.turns_failed, t.turns_interrupted), (3, 0, 0), "{t:?}");
    assert!(t.agent_ms >= t.tool_ms, "{t:?}");
    assert_eq!(t.peak_agents, 1);
    assert_eq!(r.by_day.len(), 1);
    assert_eq!(r.by_day[0].turns, 2);
    assert_eq!(r.by_model.iter().map(|m| (m.key.as_str(), m.calls)).collect::<Vec<_>>(), [("m1", 3)]);
    assert_eq!(r.by_tool.iter().map(|t| (t.key.as_str(), t.calls, t.failed)).collect::<Vec<_>>(), [("bash", 1, 0)]);
    assert_eq!(r.by_group.iter().map(|g| (g.key.as_str(), g.turns)).collect::<Vec<_>>(), [("acme", 2)]);
    assert_eq!(r.waits.replies, 1, "the correction came after a finished turn");
    let other: AnalyticsReport =
        http.get(format!("{api}/analytics?days=1&labels=client:other")).send().await.unwrap().json().await.unwrap();
    assert_eq!(other.total.turns, 0, "the label filter");

    // The marks rebuilt from the event log give the same report.
    // (One batch: all of it lands at once.)
    let db = sqlx::SqlitePool::connect(&format!("sqlite://{}", head_dir.path().join("c6h.db").display())).await.unwrap();
    sqlx::query("DELETE FROM activity").execute(&db).await.unwrap();
    sqlx::query("DELETE FROM settings WHERE key = 'activity_backfilled'").execute(&db).await.unwrap();
    assert_eq!(report().await.total.turns, 0);
    c6_head::analytics::spawn_backfill(head.state.clone());
    eventually("the marks to be rebuilt", async || {
        let again = report().await;
        (again.total.turns == 2).then_some(again)
    })
    .await;
    assert_eq!(report().await, r);

    stop.cancel();
    head.shutdown().await.unwrap();
}
