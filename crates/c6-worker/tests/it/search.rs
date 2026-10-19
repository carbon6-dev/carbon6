//! Session search: events are indexed as they arrive, the index is rebuilt
//! when its version changes, and results point at the matching events.

use std::time::Duration;

use c6_api::{Connection, Session, SessionDetail, SessionStatus};
use c6_head::HeadConfig;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

async fn eventually<T>(what: &str, mut f: impl AsyncFnMut() -> Option<T>) -> T {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(v) = f().await {
            return v;
        }
        assert!(tokio::time::Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn cfg(dir: &std::path::Path, ui: u16, worker: u16) -> HeadConfig {
    HeadConfig {
        data_dir: dir.to_owned(),
        bind: vec!["127.0.0.1".parse().unwrap()],
        ui_port: ui,
        worker_port: worker,
        auth: false,
        ..Default::default()
    }
}

async fn search(http: &reqwest::Client, api: &str, q: &[(&str, &str)]) -> Value {
    http.get(format!("{api}/search")).query(q).send().await.unwrap().json().await.unwrap()
}

#[tokio::test]
async fn search_finds_events_and_survives_reindex() {
    let mock = MockLlm::start().await;
    let head_dir = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let head = c6_head::start(cfg(head_dir.path(), 0, 0)).await.unwrap();
    let ports = (head.ui_addrs[0].port(), head.worker_addrs[0].port());
    let api = format!("http://127.0.0.1:{}/api/v1", ports.0);
    let http = reqwest::Client::new();
    let e = c6_head::api::new_enrollment(&head.state, None).await.unwrap();
    enroll::enroll(&e.token).await.unwrap().save(state.path()).unwrap();
    let worker = Worker::open(state.path(), Identity::load(state.path()).unwrap()).await.unwrap();
    let (tx, mut rx) = watch::channel(ConnState::Connecting);
    let stop = CancellationToken::new();
    tokio::spawn(conn::run(worker.clone(), tx, stop.clone()));
    tokio::time::timeout(Duration::from_secs(10), rx.wait_for(|s| matches!(s, ConnState::Attached { .. }))).await.unwrap().unwrap();
    let conn: Connection = http
        .post(format!("{api}/connections"))
        .json(&json!({"name": "m", "base_url": mock.base_url()}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    mock.push(Reply::ToolCalls(vec![("bash".into(), r#"{"command":"echo zebra_crossing_42"}"#.into())]));
    mock.push(Reply::Text("The output mentions a striped animal.".into()));
    let s: Session = http
        .post(format!("{api}/sessions"))
        .json(&json!({"worker_id": worker.identity.meta.worker_id.0, "cwd": project.path().display().to_string(),
                      "connection_id": conn.id, "model": "m", "prompt": "Find the quokka in the logs"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    eventually("turn to finish", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{}", s.id)).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Idle && d.session.last_seq >= 9).then_some(())
    })
    .await;

    // A substring of the command, in the tool call and its output.
    let r = search(&http, &api, &[("q", "ZEBRA_cross")]).await;
    assert_eq!(r["results"].as_array().unwrap().len(), 1, "{r}");
    let hit = &r["results"][0];
    assert_eq!(hit["session"]["id"], s.id.as_str());
    let kinds: Vec<&str> = hit["hits"].as_array().unwrap().iter().map(|h| h["kind"].as_str().unwrap()).collect();
    assert!(kinds.contains(&"tool_call") && kinds.contains(&"tool_result"), "{kinds:?}");
    let marked: Vec<&str> =
        hit["hits"][0]["snippet"].as_array().unwrap().iter().filter(|p| p["hit"] == true).map(|p| p["text"].as_str().unwrap()).collect();
    assert_eq!(marked, ["zebra_cross"], "the match is marked, in its original case");

    // Kind filter, phrase, exclusion, and the event a hit points at.
    let r = search(&http, &api, &[("q", "quokka"), ("kind", "user")]).await;
    let h = &r["results"][0]["hits"][0];
    assert_eq!(h["kind"], "user");
    let events: Vec<Value> = http.get(format!("{api}/sessions/{}/events", s.id)).send().await.unwrap().json().await.unwrap();
    let ev = events.iter().find(|e| e["seq"] == h["seq"]).unwrap();
    assert_eq!(ev["type"], "user");
    assert!(search(&http, &api, &[("q", "quokka"), ("kind", "assistant")]).await["results"].as_array().unwrap().is_empty());
    assert_eq!(search(&http, &api, &[("q", "\"striped animal\"")]).await["results"].as_array().unwrap().len(), 1);
    assert!(search(&http, &api, &[("q", "striped -animal")]).await["results"].as_array().unwrap().is_empty());
    let r = http.get(format!("{api}/search")).query(&[("q", "ab")]).send().await.unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::BAD_REQUEST);

    // A match in a side chat says where to open it: the main session with
    // the side chat expanded.
    let events: Vec<Value> = http.get(format!("{api}/sessions/{}/events", s.id)).send().await.unwrap().json().await.unwrap();
    let anchor_seq = events.iter().find(|e| e["type"] == "assistant").unwrap()["seq"].clone();
    mock.push(Reply::Text("Capybaras are not involved.".into()));
    let side: Session = http
        .post(format!("{api}/sessions/{}/side-chats", s.id))
        .json(&json!({"anchor_seq": anchor_seq, "connection_id": conn.id, "model": "m", "prompt": "Any capybaras?"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    eventually("side chat turn", async || {
        let d: SessionDetail = http.get(format!("{api}/sessions/{}", side.id)).send().await.ok()?.json().await.ok()?;
        (d.session.status == SessionStatus::Idle && d.queued.is_empty() && d.session.last_seq >= 5).then_some(())
    })
    .await;
    let r = search(&http, &api, &[("q", "capybaras"), ("kind", "assistant")]).await;
    let res = &r["results"][0];
    assert_eq!(res["session"]["id"], side.id.as_str());
    assert_eq!(res["open_in"]["session_id"], s.id.as_str());
    assert_eq!(res["open_in"]["side_chats"], json!([side.id]));
    assert_eq!(res["hits"][0]["anchor"], format!("{}-e{}", side.id, res["hits"][0]["seq"]));
    // Tool output points at its call's card.
    let r = search(&http, &api, &[("q", "zebra"), ("kind", "tool_result")]).await;
    assert!(r["results"][0]["hits"][0]["anchor"].as_str().unwrap().starts_with("call-"), "{r}");
    assert!(r["results"][0]["open_in"].is_null());

    // Find in session: matches in transcript order, side chats included.
    let find = |q: &str| {
        let (http, url) = (http.clone(), format!("{api}/sessions/{}/matches", s.id));
        let q = q.to_owned();
        async move { http.get(url).query(&[("q", q)]).send().await.unwrap().json::<Value>().await.unwrap() }
    };
    let m = find("zebra").await;
    let anchors: Vec<&str> = m["matches"].as_array().unwrap().iter().map(|m| m["anchor"].as_str().unwrap()).collect();
    assert_eq!(anchors.len(), 2, "{m}");
    assert!(anchors[0].starts_with(&format!("{}-e", s.id)) && anchors[1].starts_with("call-"), "the call, then its output: {anchors:?}");
    let m = find("capybara").await;
    assert_eq!(m["matches"].as_array().unwrap().len(), 2, "the question and the reply in the side chat: {m}");
    assert_eq!(m["matches"][0]["open"], json!([side.id]));
    assert_eq!(m["matches"][0]["kind"], "user");
    let m = find("striped capybara").await;
    assert!(m["matches"].as_array().unwrap().is_empty());

    // A format change re-indexes on start, without duplicates.
    stop.cancel();
    head.state.store.set_setting("search.version", "0").await.unwrap();
    head.shutdown().await.unwrap();
    let head = crate::restart_head(cfg(head_dir.path(), ports.0, ports.1)).await;
    eventually("re-index", async || (head.state.store.setting("search.version").await.unwrap().as_deref() == Some("1")).then_some(()))
        .await;
    let r = search(&http, &api, &[("q", "quokka")]).await;
    assert_eq!(r["results"][0]["total_hits"], 1, "{r}");
    head.shutdown().await.unwrap();
}
