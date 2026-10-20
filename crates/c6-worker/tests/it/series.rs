//! Series (docs/os.md §4.4): points with dimensions, from the API, agents
//! and triggers; summaries over windows, by dimension and by hour; model
//! usage as series; thresholds and "stopped arriving" as trigger events.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use c6_api::{Connection, SeriesResult};
use c6_head::HeadConfig;
use c6_testkit::eventually;
use c6_testkit::{MockLlm, Reply};
use c6_worker::{ConnState, Identity, Worker, conn, enroll};
use reqwest::StatusCode;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

async fn receiver() -> (String, Arc<Mutex<Vec<Value>>>) {
    let got: Arc<Mutex<Vec<Value>>> = Arc::default();
    let g = got.clone();
    let app = axum::Router::new().route(
        "/hook",
        axum::routing::post(move |axum::Json(v): axum::Json<Value>| {
            let g = g.clone();
            async move {
                g.lock().unwrap().push(v);
                "ok"
            }
        }),
    );
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/hook", l.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (url, got)
}

#[tokio::test]
async fn series_record_summarize_and_alert() {
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
    let base = format!("http://127.0.0.1:{}", head.ui_addrs[0].port());
    let api = format!("{base}/api/v1");
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
    use reqwest::Method as M;
    let send = |method: M, path: &str, body: Value| {
        let (http, url) = (http.clone(), format!("{api}/{path}"));
        async move { http.request(method, &url).json(&body).send().await.unwrap() }
    };
    let ok = async |r: reqwest::Response| -> Value {
        let s = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(s.is_success(), "{s}: {v}");
        v
    };
    let query = |name: &'static str, q: Value| async move {
        send(M::POST, &format!("series/{name}/query"), q).await.json::<SeriesResult>().await.unwrap()
    };
    let (hook, got) = receiver().await;
    ok(send(M::PUT, "settings/channels", json!([{"name": "phone", "kind": "webhook", "url": hook}])).await).await;

    // A series, checked.
    assert_eq!(send(M::POST, "series", json!({"name": "Bad Name"})).await.status(), StatusCode::BAD_REQUEST);
    assert_eq!(send(M::POST, "series", json!({"name": "model_spend"})).await.status(), StatusCode::BAD_REQUEST);
    ok(send(M::POST, "series", json!({"name": "revenue", "unit": "USD", "dimensions": ["service"]})).await).await;
    assert_eq!(
        send(M::POST, "series/revenue/points", json!({"value": 1, "dims": {"color": "red"}})).await.status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(send(M::POST, "series/model_calls/points", json!({"value": 1})).await.status(), StatusCode::BAD_REQUEST);

    // Points, and summaries.
    let now = c6_core::now_ms();
    for (ago_h, v, svc) in [(30, 50.0, "invoicer"), (3, 20.0, "invoicer"), (2, 5.0, "pdfs"), (1, 10.0, "invoicer")] {
        ok(send(M::POST, "series/revenue/points", json!({"ts": now - ago_h * 3_600_000, "value": v, "dims": {"service": svc}})).await)
            .await;
    }
    let day = query("revenue", json!({})).await;
    assert_eq!((day.value, day.points), (Some(35.0), 3), "the last day by default");
    let week = query("revenue", json!({"minutes": 7 * 1440, "group_by": "service"})).await;
    assert_eq!(week.value, Some(85.0));
    assert_eq!(week.groups, vec![(json!("invoicer"), 80.0), (json!("pdfs"), 5.0)]);
    let by_hour = query("revenue", json!({"bucket": "hour", "agg": "max", "where": {"service": "invoicer"}})).await;
    assert_eq!(by_hour.buckets.iter().map(|b| b.1).collect::<Vec<_>>(), vec![20.0, 10.0]);
    assert_eq!(query("revenue", json!({"agg": "avg", "minutes": 7 * 1440})).await.value, Some(21.25));
    assert_eq!(send(M::POST, "series/revenue/query", json!({"agg": "median"})).await.status(), StatusCode::BAD_REQUEST);

    // A webhook records a value; a threshold on the day's sum notifies once
    // when crossed.
    let hook_t = ok(send(
        M::POST,
        "triggers",
        json!({"name": "stripe", "trigger": "webhook", "action": "record",
               "action_params": {"series": "revenue", "value": "$.body.amount", "dims": {"service": "$.body.service"}}}),
    )
    .await)
    .await;
    ok(send(
        M::POST,
        "triggers",
        json!({"name": "good day", "trigger": "series", "event_params": {"series": "revenue", "agg": "sum", "minutes": 1440, "op": ">=", "value": 100, "ops": ["crossed"]},
               "action": "notify", "action_params": {"title": "Revenue today: {{$.value}}"}}),
    )
    .await)
    .await;
    assert_eq!(
        send(M::POST, "triggers", json!({"name": "x", "trigger": "series", "event_params": {"series": "revenue"}, "action": "notify", "action_params": {"title": "t"}}))
            .await
            .status(),
        StatusCode::BAD_REQUEST,
        "a threshold needs op and value"
    );
    let url = format!("{base}{}", hook_t["webhook_url"].as_str().unwrap());
    http.post(&url).json(&json!({"amount": 40, "service": "pdfs"})).send().await.unwrap();
    assert!(!got.lock().unwrap().iter().any(|n| n["title"].as_str().is_some_and(|t| t.starts_with("Revenue today"))));
    http.post(&url).json(&json!({"amount": "30", "service": "pdfs"})).send().await.unwrap();
    let note = eventually("the threshold", async || {
        got.lock().unwrap().iter().find(|n| n["title"].as_str().is_some_and(|t| t.starts_with("Revenue today"))).cloned()
    })
    .await;
    assert_eq!(note["title"], "Revenue today: 105");
    http.post(&url).json(&json!({"amount": 1, "service": "pdfs"})).send().await.unwrap();
    assert_eq!(
        got.lock().unwrap().iter().filter(|n| n["title"].as_str().is_some_and(|t| t.starts_with("Revenue today"))).count(),
        1,
        "once, while crossed"
    );

    // Stopped arriving: a heartbeat that goes quiet, then comes back.
    ok(send(M::POST, "series", json!({"name": "heartbeat", "dimensions": ["service"]})).await).await;
    ok(send(
        M::POST,
        "triggers",
        json!({"name": "quiet", "trigger": "series", "event_params": {"series": "heartbeat", "silent_minutes": 0.001, "where": {"service": "pdfs"}},
               "action": "notify", "action_params": {"title": "heartbeat {{$.op}}"}}),
    )
    .await)
    .await;
    tokio::time::sleep(Duration::from_millis(150)).await;
    c6_head::triggers::watch_series(&head.state, None, None).await;
    eventually("silent", async || got.lock().unwrap().iter().any(|n| n["title"] == "heartbeat crossed").then_some(())).await;
    ok(send(M::POST, "series/heartbeat/points", json!({"value": 1, "dims": {"service": "pdfs"}})).await).await;
    eventually("back", async || got.lock().unwrap().iter().any(|n| n["title"] == "heartbeat cleared").then_some(())).await;

    // Agents record and read; model usage is a series.
    mock.push(Reply::ToolCalls(vec![(
        "series_record".into(),
        json!({"series": "revenue", "value": 7, "dims": {"service": "pdfs"}}).to_string(),
    )]));
    mock.push(Reply::ToolCalls(vec![("series_query".into(), json!({"series": "revenue", "query": {"group_by": "service"}}).to_string())]));
    mock.push(Reply::Text("Recorded.".into()));
    ok(send(M::POST, "sessions", json!({"worker_id": wid, "cwd": "", "connection_id": conn.id, "model": "m", "prompt": "log a sale"}))
        .await)
    .await;
    let seen = eventually("the agent's query", async || {
        let last = mock.requests().last()?["messages"].to_string();
        last.contains("\\\"groups\\\"").then_some(last)
    })
    .await;
    assert!(seen.contains("pdfs"), "{seen}");
    let points: Value = send(M::GET, "series/revenue", json!({})).await.json().await.unwrap();
    assert_eq!(points["points"], 8, "{points}");
    eventually("model usage", async || (query("model_calls", json!({})).await.value? >= 3.0).then_some(())).await;
    let list: Value = send(M::GET, "series", json!({})).await.json().await.unwrap();
    let names: Vec<&str> = list.as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    for n in ["host_up", "host_check_ms", "revenue", "heartbeat", "model_spend", "model_tokens", "model_calls"] {
        assert!(names.contains(&n), "{n} in {names:?}");
    }
    assert_eq!(send(M::DELETE, "series/host_up", json!({})).await.status(), StatusCode::BAD_REQUEST);

    stop.cancel();
    head.shutdown().await.unwrap();
}
